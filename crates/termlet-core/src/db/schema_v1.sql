-- 结构版本 1。Termlet 正式发布之前可以直接修改这一版（没有用户数据）；发布之后只能在后面追加迁移。
-- 所有表 STRICT；时间一律是 Unix 毫秒。

-- 内部元数据：数据目录 ID、保险库的模式与校验值、主密码的派生参数等
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) STRICT;

-- 条目表（可同步的数据），列都相同：
--   parent_id  所属分组等关系（没有关系的类为 NULL）
--   sort_key   分数索引，按字节比较即位置（见 sort_key.rs）
--   doc        条目内容（JSON），不含任何秘密；加选项只加带默认值的字段，不改表
--   deleted_at 删除墓碑：同步需要知道哪些条目被删了；墓碑的 doc 为 '{}'
-- 条目之间的引用（主机引用凭据档案、跳板机等）由存储层维护，不加外键：同步时条目可能先于它引用的条目到达。
CREATE TABLE host (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;
CREATE INDEX host_parent ON host (parent_id);

CREATE TABLE host_group (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;

CREATE TABLE profile (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;

CREATE TABLE ssh_key (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;

CREATE TABLE snippet (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;

CREATE TABLE forward (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;

CREATE TABLE workspace (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;

CREATE TABLE color_scheme (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sort_key   TEXT NOT NULL,
  doc        TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;

-- 设置：每组一行（id 为组名，如 terminal、connection），同步按组合并
CREATE TABLE setting (
  id         TEXT PRIMARY KEY,
  doc        TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- 只与这台电脑有关、不同步的设置（本地终端、CLI 开关、自动快照等），每组一行
CREATE TABLE local_setting (
  id         TEXT PRIMARY KEY,
  doc        TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- 秘密：属于某个条目的某一项（owner 为 "host/h_…"，field 为 password、private_key 等）。
-- blob 是保险库加密后的 nonce 与密文，附加数据绑定 owner 与 field，两条秘密互换位置会解密失败。
CREATE TABLE secret (
  owner      TEXT NOT NULL,
  field      TEXT NOT NULL,
  blob       BLOB NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (owner, field)
) STRICT, WITHOUT ROWID;

-- 已确认的主机指纹（不同步：每台电脑各自确认）。一个地址可有多种算法的密钥，连接时优先请求已记录的那种
CREATE TABLE known_host (
  host       TEXT NOT NULL,
  port       INTEGER NOT NULL,
  algorithm  TEXT NOT NULL,
  key        TEXT NOT NULL, -- OpenSSH 格式的公钥（算法 base64）
  added_at   INTEGER NOT NULL,
  PRIMARY KEY (host, port, algorithm)
) STRICT, WITHOUT ROWID;

-- 命令历史（不同步：只在本机）。命令由保险库加密，附加数据绑定 owner（主机 ID、quick:…、local:…）
CREATE TABLE command_history (
  id         INTEGER PRIMARY KEY,
  owner      TEXT NOT NULL,
  command    BLOB NOT NULL,
  used_at    INTEGER NOT NULL
) STRICT;
CREATE INDEX command_history_owner ON command_history (owner, used_at);

-- 审计日志（不同步；完整备份可选是否包含）：termlet 命令行工具的调用与主机工具面板的写操作，见 audit.rs。
-- 内容（detail）是命令原文或传输的路径，不含秘密；action、status 的取值见 AuditAction、AuditStatus
CREATE TABLE audit_log (
  id          INTEGER PRIMARY KEY,
  at          INTEGER NOT NULL DEFAULT 0,
  host_id     TEXT NOT NULL DEFAULT '',
  host_name   TEXT NOT NULL DEFAULT '',
  action      TEXT NOT NULL DEFAULT '',
  detail      TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT '',
  exit_code   INTEGER,
  error_code  TEXT,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  bytes       INTEGER,
  caller      TEXT NOT NULL DEFAULT ''
) STRICT;
