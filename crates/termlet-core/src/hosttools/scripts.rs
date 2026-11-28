//! 远端脚本。
//!
//! 都经 [`super::exec::sh_script`] 以 `sh -c` 执行（登录 shell 是 fish、tcsh 时也能用），远端数据与参数经 `$1…` 传入，
//! 不拼进脚本；开头 `export LC_ALL=C`（输出格式不随语言变化），并把 sbin 目录补进 PATH（CentOS 7 等经 exec 通道
//! 执行时 PATH 里没有 /usr/sbin，ss 在那里）。只读 /proc 与各命令的输出，不往服务器写任何文件。
//!
//! 为便于阅读写成多行，执行前由 [`super::exec::one_line`] 合成一行（tcsh 的单引号内不能有换行）：因此每行都以分号、
//! do、then、else、in、|、&&、|| 等结尾，脚本中不写注释，也不用 `!`（csh 的历史替换），不等写成 `-ne`。
//!
//! 防伪造段标记：输出按 `@@段名` 分段（见 [`super::sections`]）。远端可控的内容（进程的可执行文件与当前目录、
//! `/proc/<pid>/stat` 中的进程名、命令行、ss 输出中的进程名、服务描述、容器的输出与错误等）一律经 `q` 在每行前加一个
//! 空格再输出，只有脚本自己 echo 的标记行以 `@@` 开头；解析时每个内容行去掉开头的一个空格。这样内容里的换行与 `@@`
//! 都只是内容。不经 `q` 的只有脚本自己产生的、内核与系统给出的固定格式（`id -u`、getconf、/proc/stat、/proc/net、
//! /etc/passwd 的名称与 UID 等），以及进程列表的循环（逐个进程把 stat 的各行接成一行再输出，进程名中的换行不会成为
//! 新的一行）。

/// 每个脚本的开头：`q` 在每行前加一个空格（远端可控的内容经它输出，见上）。
pub(crate) const PRELUDE: &str = r#"export LC_ALL=C;
PATH=$PATH:/usr/sbin:/sbin:/usr/local/sbin; export PATH;
q() { sed 's/^/ /'; };
"#;

/// 列出进程（读 /proc，不依赖 ps 的参数差异，BusyBox 也能用）。`$1` 是终端的会话标记（`LC_TERMLET_SESSION=…`，
/// 可为空）：环境变量中带它的进程是这个终端的 shell 及其子进程（`@@self`）。`$2` 是会话标记的值（可为空）：
/// tmux 终端的 tmux 会话以它命名，`@@tmux` 列出这个会话各窗格的 shell（`会话名:PID`）。tmux 中的 shell 继承的是
/// tmux 服务的环境（第一个启动 tmux 的终端的标记），环境变量中的标记不准，有 `@@tmux` 时以它为准。
/// 每个进程一行：PID 状态 父进程 utime stime 优先级 nice 线程数 启动时刻 虚拟内存 常驻页数 UID 名称（名称中有换行时
/// stat 不止一行，第一行的字段不够：改为整个读出、换行写成 `?`（与 ps 相同），进程不会被漏掉，也不会多出一行）；
/// 排除内核线程（flags 含 PF_KTHREAD），以及这个脚本自己的 shell（`$$`，记在 `me` 中）与它派生的子进程（父进程是
/// `$$` 的，命令替换的子 shell 等）：它们每次读取都是新的 PID，不是用户的进程。最多 2000 个，`@@total` 是实际个数。
/// `@@cgroup` 是各进程的 cgroup 路径（所属的服务与容器，见 [`super::cgroup`]）：一次 grep 读全部进程的
/// `/proc/<pid>/cgroup`（不在循环中逐个读，2000 个进程时只多二三十毫秒），`-m 1` 每个文件只取第一个匹配的行——
/// v1 的各控制器都带着容器的路径，容器的那一行在前；不在容器中时是 `name=systemd`（v1）或 `0::`（v2 与混合模式）
/// 的那一行。只留下带服务名或容器 ID 的（登录会话、根 cgroup 中的进程不输出）；同一个路径只写一次（`=编号 路径`），
/// 各进程写“PID 编号”（同一个容器的进程很多时输出不随之成倍增加）。经 `q` 输出。
/// `@@cmdline` 是各进程的命令行：一次 head 读全部进程的 `/proc/<pid>/cmdline`，每个只读开头 [`CMDLINE_MAX`] 字节，
/// 各自前面有 head 写的 `==> 路径 <==`（grep 不行：BusyBox 的 grep 读到第一个 NUL 就截断，只剩程序名），参数之间的
/// NUL 换成空格，经 `q` 输出，最后补一个换行（最后一个命令行没有换行，下一个段标记仍在新的一行）。参数中的换行把命令行
/// 分成几行，解析时接起来；参数中写进的 `==> …/cmdline <==` 冒充不了别的进程：每个进程本来就有自己的一行标题，同一个
/// PID 出现两次时都不认。
pub(crate) const PROC_BODY: &str = r#"if [ -r /proc/stat ]; then :; else echo @@unsupported; uname -s; exit 0; fi;
echo @@uid; id -u;
echo @@clk; getconf CLK_TCK 2>/dev/null || echo 100;
echo @@page; getconf PAGESIZE 2>/dev/null || echo 4096;
echo @@uptime; cat /proc/uptime;
echo @@stat; grep -E '^(btime|cpu[0-9])' /proc/stat;
echo @@mem; grep '^MemTotal:' /proc/meminfo;
echo @@passwd; cut -d: -f1,3 /etc/passwd 2>/dev/null;
echo @@self; if [ -n "$1" ]; then grep -l -F "$1" /proc/[0-9]*/environ 2>/dev/null; fi;
echo @@tmux; if [ -n "$2" ] && command -v tmux >/dev/null 2>&1; then tmux list-panes -s -t "$2" -F '#{session_name}:#{pane_pid}' 2>/dev/null | q; fi;
echo @@cgroup; grep -H -m 1 -E '^0::/|:name=systemd:/|docker|libpod|containerd|kubepods|crio|nerdctl' /proc/[0-9]*/cgroup 2>/dev/null | grep -E '\.service|[0-9a-f]{64}' | awk '{ i = index($0, "/cgroup:"); p = substr($0, 1, i - 1); sub(/.*\//, "", p); r = substr($0, i + 8); r = substr(r, index(r, ":") + 1); r = substr(r, index(r, ":") + 1); if (r in s) k = s[r]; else { k = ++n; s[r] = k; print "=" k " " r; } print p " " k; }' | q;
echo @@cmdline; head -c @CMDLINE@ /proc/[0-9]*/cmdline 2>/dev/null | tr '\000' ' ' | q; echo;
echo @@procs; n=0; me=$$;
for d in /proc/[0-9]*; do
  read -r s < "$d/stat" || continue;
  set -- ${s##*) };
  if [ $# -lt 20 ]; then s=$(tr '\n' '?' < "$d/stat" 2>/dev/null); s=${s%?}; set -- ${s##*) }; fi;
  [ "$7" -ge 0 ] || continue;
  [ $(( $7 & 2097152 )) -eq 0 ] || continue;
  [ "${d#/proc/}" -ne "$me" ] || continue;
  [ "$2" -ne "$me" ] || continue;
  n=$((n+1));
  [ $n -le 2000 ] || continue;
  u=-1;
  while read -r k v w; do if [ "$k" = Uid: ]; then u=$v; break; fi; done < "$d/status";
  c=${s#*(}; c=${c%) *};
  printf '%s %s %s %s %s %s %s %s %s %s %s %s %s\n' "${d#/proc/}" "$1" "$2" "${12}" "${13}" "${16}" "${17}" "${18}" "${20}" "${21}" "${22}" "$u" "$c";
done 2>/dev/null;
echo "@@total $n""#;

/// 读取一个进程（`$1` 为 PID）的详情；进程不存在时以退出码 3 结束。stat（进程名可含换行）、status、
/// 可执行文件与当前目录（路径可含换行）、命令行都经 `q` 输出。
pub(crate) const PROC_DETAIL_BODY: &str = r#"[ -d "/proc/$1" ] || exit 3;
echo @@stat; cat "/proc/$1/stat" | q;
echo @@status; cat "/proc/$1/status" | q;
echo @@exe; readlink "/proc/$1/exe" | q;
echo @@cwd; readlink "/proc/$1/cwd" | q;
echo @@btime; grep '^btime' /proc/stat;
echo @@clk; getconf CLK_TCK 2>/dev/null || echo 100;
echo @@page; getconf PAGESIZE 2>/dev/null || echo 4096;
echo @@passwd; cut -d: -f1,3 /etc/passwd 2>/dev/null;
echo @@cmdline; head -c 65536 "/proc/$1/cmdline" | tr '\000' ' ' | q"#;

/// 列出 systemd 服务：服务名取 list-unit-files 与 list-units --all 的并集（排除模板 `*@.service`），
/// 再用一次 systemctl show 取全部状态（时间以 UTC 写出，`TZ=UTC`，解析时不必猜服务器的时区）。
/// 没有 systemd 时输出 `@@nosystemd`（之后一行是 `uname -s`）。is-system-running 的标准错误（连不上 D-Bus 等，
/// 远端的原文）经 `q` 输出在 `@@stderr`：列表为空时据此说明原因。
/// 定时器（服务页的“定时器”子页）一并读出：`@@timers` 是 `list-timers --all` 的输出（下次、上次触发的时间与触发的
/// 单元；按相对时间触发的定时器，下次的时间只有它算好了），`@@tshow` 是定时器的属性。定时器名取 list-unit-files
/// 与 list-timers 的并集（没有启用、没有加载的定时器 list-timers 不列出），排除模板。都是只读的。
/// 服务器的时区（日历表达式按它解释）：`@@tz` 是时区名（`/etc/localtime` 链接到的 zoneinfo 名称，没有时取
/// `/etc/timezone`，再没有时问 `timedatectl show -p Timezone`），`@@tzoffset` 是现在的偏移（`date +%z`），`@@tzabbr`
/// 是现在的缩写；`@@ltimers` 是不带 `TZ=UTC` 的同一份 list-timers（按服务器时区写出的时间），与 `@@timers` 对照得出
/// 各个时刻的偏移（夏令时前后不同）。
pub(crate) const SERVICE_BODY: &str = r#"[ -d /run/systemd/system ] || { echo @@nosystemd; uname -s; exit 0; };
echo @@uid; id -u;
echo @@version; systemctl --version 2>/dev/null | head -n 1;
echo @@state; { e=$(systemctl is-system-running 2>&1 1>&3); } 3>&1;
echo @@stderr; [ -z "$e" ] || printf '%s\n' "$e" | head -n 5 | q;
n=$( { systemctl list-unit-files --type=service --no-legend --no-pager 2>/dev/null; systemctl list-units --type=service --all --plain --no-legend --no-pager 2>/dev/null; } | awk '{for(i=1;i<=NF;i++) if ($i ~ /\.service$/) {print $i; break}}' | grep -v '@\.service$' | sort -u );
echo @@show;
[ -z "$n" ] || TZ=UTC systemctl show --no-pager -p Id -p Description -p LoadState -p ActiveState -p SubState -p UnitFileState -p FragmentPath -p MainPID -p Result -p ExecMainCode -p ExecMainStatus -p ActiveEnterTimestamp -p StateChangeTimestamp -p NRestarts -p MemoryCurrent -p CPUUsageNSec -p TasksCurrent -- $n | q;
l=$(TZ=UTC systemctl list-timers --all --no-legend --no-pager 2>/dev/null);
echo @@timers; [ -z "$l" ] || printf '%s\n' "$l" | q;
echo @@ltimers; [ -z "$l" ] || systemctl list-timers --all --no-legend --no-pager 2>/dev/null | q;
echo @@tz; z=$(readlink /etc/localtime 2>/dev/null); z=${z#*zoneinfo/};
case $z in /*) z=;; esac;
[ -n "$z" ] || z=$(head -n 1 /etc/timezone 2>/dev/null);
[ -n "$z" ] || z=$(timedatectl show -p Timezone --value 2>/dev/null);
printf '%s\n' "$z" | head -n 1 | q;
echo "@@tzoffset $(date +%z 2>/dev/null)";
echo @@tzabbr; date +%Z 2>/dev/null | q;
m=$( { systemctl list-unit-files --type=timer --no-legend --no-pager 2>/dev/null; printf '%s\n' "$l"; } | awk '{for(i=1;i<=NF;i++) if ($i ~ /\.timer$/) {print $i; break}}' | grep -v '@\.timer$' | sort -u );
echo @@tshow;
[ -z "$m" ] || TZ=UTC systemctl show --no-pager -p Id -p Description -p LoadState -p ActiveState -p SubState -p UnitFileState -p FragmentPath -p Unit -p NextElapseUSecRealtime -p LastTriggerUSec -p TimersCalendar -p TimersMonotonic -p Persistent -p RandomizedDelayUSec -p Result -- $m | q"#;

/// 访问 Docker（Docker 页与进程页读容器名共用）：有 docker 时用 docker，否则用 podman；都没有时输出 `@@nodocker`。
/// 以 `$f` 为格式列出容器（`$o`）；普通用户没有权限（不在 docker 组）时，免密码 sudo 可用就改用 `sudo -n`
/// （`@@sudo used`，之后的命令都经 `$d` 带上 sudo），否则输出 `@@sudo` 与原因、`@@error` 与输出后结束。出错时 docker
/// 是 Docker Desktop 的（WSL 中指向 /mnt/wsl/docker-desktop 的链接，或 Windows 一侧 Docker Desktop 的程序）先输出
/// `@@desktop`：连不上守护进程时下一步在 Windows 一侧（启动 Docker Desktop、为发行版打开 WSL 集成）。
pub(crate) const DOCKER_ACCESS: &str = r#"if command -v docker >/dev/null 2>&1; then d=docker; elif command -v podman >/dev/null 2>&1; then d=podman; else echo @@nodocker; uname -s; exit 0; fi;
echo "@@engine $d";
o=$($d ps -a --no-trunc --format "$f" 2>&1); r=$?;
if [ $r -ne 0 ] && [ "$(id -u)" -ne 0 ]; then
  case $o in *"ermission denied"*)
    if command -v sudo >/dev/null 2>&1; then
      q=$(sudo -n $d ps -a --no-trunc --format "$f" 2>&1); x=$?;
      case $x:$q in
        0:*) d="sudo -n $d"; o=$q; r=0; echo "@@sudo used";;
        *"password is required"*|*"terminal is required"*|*"must have a tty"*) echo "@@sudo password";;
        *"sudoers"*|*"not allowed to"*|*"may not run sudo"*) echo "@@sudo denied";;
        *) d="sudo -n $d"; o=$q; r=$x; echo "@@sudo used";;
      esac;
    else echo "@@sudo none"; fi;;
  esac;
fi;
if [ $r -ne 0 ]; then
  case $(readlink -f "$(command -v docker)" 2>/dev/null) in /mnt/wsl/docker-desktop/*|*/Docker/resources/bin/*) echo @@desktop;; esac;
  echo "@@error $r"; printf '%s\n' "$o" | q; exit 0;
fi;
"#;

/// 列出容器、镜像、卷、网络（接在 [`DOCKER_ACCESS`] 之后，`$f` 为 `{{json .}}`）。`@@inspect` 每个容器一行：
/// ID|镜像 ID|挂载的卷,…|所在网络,…|`{"labels":标签,"state":运行状态}`（JSON 对象）。前四项用来标出使用中的镜像、
/// 卷、网络（只取卷的名称：docker 的绑定挂载没有 Name，模板写 `.Name` 时整个容器的这一行出错，它的 compose 项目也就
/// 认不出了）；compose 项目等标签取自这里的 JSON（docker ps 的 Labels 是以逗号连接的字符串，标签值中写进逗号就能伪造
/// 别的标签）；运行状态（启动与结束的时间、退出码、健康状态）让列表写出本地化的状态与时长，与详情一致（`.State` 整个
/// 取出：podman 较早的版本把健康状态叫 Healthcheck，模板写 `.State.Health` 时整行出错）。
/// `@@version` 是引擎的版本（守护进程的，podman 没有时取客户端的），`@@compose` 是 Compose 的版本（`docker compose`
/// 插件，没有时取 `docker-compose`），都只取第一行、经 `q` 输出，读不到时为空：信息行写出它们，不为此另外执行一次。
pub(crate) const DOCKER_BODY: &str = r#"echo @@version; v=$($d version --format '{{.Server.Version}}' 2>/dev/null); [ -n "$v" ] || v=$($d version --format '{{.Client.Version}}' 2>/dev/null); printf '%s\n' "$v" | head -n 1 | q;
echo @@compose; { $d compose version --short 2>/dev/null || docker-compose version --short 2>/dev/null; } | head -n 1 | q;
echo @@containers; printf '%s\n' "$o" | q;
echo @@images; $d images --no-trunc --format '{{json .}}' 2>/dev/null | q;
echo @@volumes; $d volume ls --format '{{json .}}' 2>/dev/null | q;
echo @@networks; $d network ls --no-trunc --format '{{json .}}' 2>/dev/null | q;
echo @@inspect; ids=$($d ps -aq --no-trunc 2>/dev/null);
[ -z "$ids" ] || $d container inspect --format '{{.Id}}|{{.Image}}|{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}},{{end}}{{end}}|{{range $k, $v := .NetworkSettings.Networks}}{{$k}},{{end}}|{"labels":{{json .Config.Labels}},"state":{{json .State}}}' $ids 2>/dev/null | q"#;

/// Docker 页的清理（接在 [`DOCKER_ACCESS`] 之后，`$f` 为 `{{.ID}}`，只读）：`@@api` 是守护进程的 API 版本（1.42 起，即
/// Docker 23 起 `volume prune` 只清理匿名卷）；`@@df` 是 `system df -v` 的 JSON（各镜像独有的大小、各容器可写层的大小、
/// 各卷的大小与标签、构建缓存；一行，读不到时为空——Podman、较早的 Docker 不支持这个写法；它的最后没有换行，补一个，
/// 下一个段标记仍在新的一行），有 `timeout` 时最多等 60 秒（卷很大时 df 很慢，超时只是没有大小）；`@@dangling` 是悬空镜像的 ID（`image prune` 清理的就是这些中没有
/// 容器使用的）；`@@volumes`、`@@networks` 是卷与网络的列表；`@@endpoints` 每个网络一行“ID|正连着它的容器 ID,…”
/// （Docker 的 `network prune` 只看正连着的，停止的容器所在的网络也会被清理；Podman 没有这一项）；`@@inspect 总数` 每个
/// 容器一行“ID|镜像 ID|状态|卷,…|网络,…|名称”（总数是 `ps -aq` 的个数，据此知道读全了没有）。内容都经 `q` 输出。
pub(crate) const CLEANUP_BODY: &str = r#"echo @@api; $d version --format '{{.Server.APIVersion}}' 2>/dev/null | head -n 1 | q;
t=; if command -v timeout >/dev/null 2>&1; then t="timeout 60"; fi;
echo @@df; $t $d system df -v --format '{{json .}}' 2>/dev/null | head -n 1 | q; echo;
echo @@dangling; $d images -f dangling=true --no-trunc -q 2>/dev/null | q;
echo @@volumes; $d volume ls --format '{{json .}}' 2>/dev/null | q;
echo @@networks; $d network ls --no-trunc --format '{{json .}}' 2>/dev/null | q;
echo @@endpoints; n=$($d network ls -q --no-trunc 2>/dev/null);
[ -z "$n" ] || $d network inspect --format '{{.Id}}|{{range $k, $v := .Containers}}{{$k}},{{end}}' $n 2>/dev/null | q;
ids=$($d ps -aq --no-trunc 2>/dev/null); echo "@@inspect $(printf '%s\n' "$ids" | grep -c .)";
[ -z "$ids" ] || $d container inspect --format '{{.Id}}|{{.Image}}|{{.State.Status}}|{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}},{{end}}{{end}}|{{range $k, $v := .NetworkSettings.Networks}}{{$k}},{{end}}|{{.Name}}' $ids 2>/dev/null | q"#;

/// 网络页最多列出的连接数；脚本中取前 `MAX_CONNS + 1` 行（含表头）。
pub(crate) const MAX_CONNS: usize = 3000;

/// 列出 TCP、UDP 连接：有 ss 时用 `ss -tuanpe`（`@@counts` 是各协议、各状态的总数），没有时读 /proc/net
/// （拿不到进程名）。ss 不转义进程名（可含换行），经 `q` 输出；解析时把不以协议开头的行接到上一行。
/// `$1` 为 `sudo` 时（有免密码 sudo，见 [`super::privilege`]）先经 `sudo -n` 读，能看到别的用户的进程的进程名
/// （`@@sudo used`）；失败时退回不带 sudo 的读法。总数不需要 sudo。
pub(crate) const CONN_BODY: &str = r#"echo @@uid; id -u;
echo @@passwd; cut -d: -f1,3 /etc/passwd 2>/dev/null;
if command -v ss >/dev/null 2>&1 && ss -tuan >/dev/null 2>&1; then
  echo @@counts; ss -tuan 2>/dev/null | awk 'NR>1{c[$1" "$2]++} END{for(k in c) print k, c[k]}' | q;
  o=; if [ "$1" = sudo ]; then o=$(sudo -n ss -tuanpe 2>/dev/null) || o=; fi;
  if [ -n "$o" ]; then echo "@@sudo used"; else o=$(ss -tuanpe 2>/dev/null); fi;
  echo @@ss; printf '%s\n' "$o" | head -n 3001 | q;
elif [ -r /proc/net/tcp ]; then
  for f in tcp tcp6 udp udp6; do
    echo "@@count $f"; awk 'NR>1{c[$4]++} END{for(k in c) print k, c[k]}' /proc/net/$f 2>/dev/null;
    echo "@@proc $f"; head -n 3001 /proc/net/$f 2>/dev/null;
  done;
else echo @@unsupported; uname -s; fi"#;

/// 定义 `p`：执行 `"$@"`；失败、输出像是权限不足、当前用户不是 root 时，改用 `sudo -n` 再执行一次。
/// 输出 `@@rc 退出码`、`@@sudo 结果`（空：没有用 sudo；used：用了；password：sudo 要密码；denied：不允许；
/// none：没有 sudo），`@@out` 之后是命令的输出（stdout 与 stderr，在标记之后，伪造不了前面的 `@@rc`、`@@sudo`）。
/// 只用免密码 sudo，从不传密码。注意 password 也包括"根本不在 sudoers 中"：sudo 先验证密码、再告诉用户不在
/// sudoers 中，`-n` 时只会说要密码（`sudo -n -l` 也一样），无法区分，界面上一并说明。
pub(crate) const PRIV_FUNC: &str = r#"p() {
  o=$("$@" 2>&1); r=$?; s=;
  if [ $r -ne 0 ] && [ "$(id -u)" -ne 0 ]; then
    case $o in *"ermission denied"*|*"not permitted"*|*"Access denied"*|*"uthentication required"*|*"must be root"*|*"must be run as root"*|*"requires root"*|*"superuser"*|*"are not allowed"*)
      if command -v sudo >/dev/null 2>&1; then
        q=$(sudo -n "$@" 2>&1); x=$?;
        case $x:$q in
          0:*) s=used; o=$q; r=0;;
          *"password is required"*|*"terminal is required"*|*"must have a tty"*) s=password;;
          *"sudoers"*|*"not allowed to"*|*"may not run sudo"*) s=denied;;
          *) s=used; o=$q; r=$x;;
        esac;
      else s=none; fi;;
    esac;
  fi;
  echo "@@rc $r"; echo "@@sudo $s"; echo @@out; printf '%s\n' "$o";
};
"#;

/// 以 `p` 执行参数给出的命令（argv 按操作拼出，各项已校验）。
pub(crate) const ACTION_BODY: &str = r#"p "$@""#;

/// 结束进程前核对启动时刻：`$1` 是 PID，`$2` 是列表中读到的启动时刻（`/proc/<pid>/stat` 的第 22 个字段，开机后的
/// 时钟周期数），之后是要执行的命令。进程已不存在，或启动时刻不同（列表过时，PID 已被别的进程重新使用）时只输出
/// `@@gone`，不执行。`st` 在子 shell 中执行，其中的 `set --` 不影响外面的参数。
pub(crate) const KILL_BODY: &str = r#"st() { f=/proc/$1/stat; read -r s < "$f" || return 0; set -- ${s##*) };
  if [ $# -lt 20 ]; then s=$(tr '\n' '?' < "$f"); s=${s%?}; set -- ${s##*) }; fi; echo "${20}"; };
k=$1; t=$2; shift 2;
if [ "$(st "$k" 2>/dev/null)" = "$t" ]; then p "$@"; else echo @@gone; fi"#;

/// 读日志、资源占用、容器详情时保留的输出：命令的输出（标准输出与标准错误）只留最后 [`super::LOG_LIMIT`] 字节
/// （日志太长时留下最新的，不在服务器上先存下全部），再加上一个换行（输出的最后一行没有换行时，`@@rc` 仍在新的一行）
/// 与最后一行 `@@rc 退出码`。
pub(crate) const READ_TAIL: usize = super::LOG_LIMIT + RC_MARGIN;
/// 最后一行 `@@rc 退出码` 占的字节数的上限。
pub(crate) const RC_MARGIN: usize = 32;

/// 读服务日志：`$@` 是 journalctl 的命令（各项已校验）。不是 root 且免密码 sudo 能执行 journalctl 时经 sudo 读
/// （能看到全部日志），否则以当前用户读，能看多少算多少。第一行 `@@sudo 结果 UID 组`（脚本自己 echo 的；在能读全部
/// 日志的 systemd-journal、adm、wheel 组中时“组”为 `journal`，否则为空），之后是命令的输出与最后一行 `@@rc 退出码`，
/// 一起经 `tail -c` 只留最后 [`READ_TAIL`] 字节：日志的内容在两者之间，伪造不了它们（解析时取第一行与最后一行）。
pub(crate) const SERVICE_LOG_BODY: &str = r#"s=;
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1 && sudo -n "$1" --version >/dev/null 2>&1; then s=used; fi;
g=; for x in $(id -Gn 2>/dev/null); do case $x in systemd-journal|adm|wheel) g=journal;; esac; done;
echo "@@sudo $s $(id -u) $g";
if [ -n "$s" ]; then set -- sudo -n "$@"; fi;
{ "$@" 2>&1; r=$?; echo; echo "@@rc $r"; } | tail -c @TAIL@"#;

/// 读取（容器日志、资源占用、容器详情）：执行 `$@`（各项已校验；Docker 页经 sudo 访问时以 `sudo -n` 开头），
/// 输出与 [`SERVICE_LOG_BODY`] 相同，只是没有第一行 `@@sudo`。
pub(crate) const READ_BODY: &str = r#"{ "$@" 2>&1; r=$?; echo; echo "@@rc $r"; } | tail -c @TAIL@"#;

/// 进入容器后执行的命令：有 bash 用 bash，否则用 sh。
pub(crate) const CONTAINER_SHELL: &str = "command -v bash >/dev/null && exec bash || exec sh";

/// 进程列表中每个进程的命令行最多读的字节数。
pub(crate) const CMDLINE_MAX: usize = 400;

/// 带开头的完整脚本。
pub(crate) fn proc_script() -> String {
    format!("{PRELUDE}{PROC_BODY}").replace("@CMDLINE@", &CMDLINE_MAX.to_string())
}

pub(crate) fn proc_detail_script() -> String {
    format!("{PRELUDE}{PROC_DETAIL_BODY}")
}

pub(crate) fn service_script() -> String {
    format!("{PRELUDE}{SERVICE_BODY}")
}

pub(crate) fn docker_script() -> String {
    [PRELUDE, "f='{{json .}}';\n", DOCKER_ACCESS, DOCKER_BODY].concat()
}

/// 进程页读容器名（只要容器的 ID 与名称，经 sudo 与否与 Docker 页的判断相同）：`@@names` 之后每个容器一行
/// “ID 名称”，经 `q` 输出。
pub(crate) fn container_names_script() -> String {
    [
        PRELUDE,
        "f='{{.ID}} {{.Names}}';\n",
        DOCKER_ACCESS,
        r#"echo @@names; printf '%s\n' "$o" | q"#,
    ]
    .concat()
}

pub(crate) fn cleanup_script() -> String {
    [PRELUDE, "f='{{.ID}}';\n", DOCKER_ACCESS, CLEANUP_BODY].concat()
}

pub(crate) fn conn_script() -> String {
    format!("{PRELUDE}{CONN_BODY}")
}

pub(crate) fn action_script() -> String {
    format!("{PRELUDE}{PRIV_FUNC}{ACTION_BODY}")
}

pub(crate) fn kill_script() -> String {
    format!("{PRELUDE}{PRIV_FUNC}{KILL_BODY}")
}

pub(crate) fn service_log_script() -> String {
    format!("{PRELUDE}{SERVICE_LOG_BODY}").replace("@TAIL@", &READ_TAIL.to_string())
}

pub(crate) fn read_script() -> String {
    format!("{PRELUDE}{READ_BODY}").replace("@TAIL@", &READ_TAIL.to_string())
}
