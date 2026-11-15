//! 会换行的中文、日文、韩文文字：照避头尾规则换行，韩文按词换行（[`prose`]）。
//!
//! GPUI 的换行（`LineWrapper::wrap_line`）在每个“非单词字符”前都可以断行，中文的标点不算单词字符，于是
//! “看不到處理程序。”在宽度正好时会把“。”单独甩到下一行行首。断不断由标点本身决定，在它前面插入 U+2060、U+00A0
//! 一类不可断字符也拦不住，GPUI 又没有可调的接口，所以在这一层另做一个文字元素：先按 GPUI 的规则找断点，再按
//! 避头尾规则往前挪（[`adjust_break`]），把标点前的一个字一起带到下一行（推出式避头尾）；挪不动（整行都是
//! 禁则标点）时保留 GPUI 的断点。断点只会往前挪，行不会比原来宽，不会溢出。
//!
//! - 行首禁则（不放在行首）：。，、；：？！ 与各种右括号、右引号、省略号、间隔号等，以及日文的小写假名（っ、ャ 等）、
//!   长音“ー”、叠字符号（々、ゝ 等）（[`no_line_start`]）。
//! - 行尾禁则（不留在行尾）：各种左括号、左引号（[`no_line_end`]）。
//! - 不可分：破折号“——”“――”、省略号“……”“‥‥”不拆到两行。
//! - 韩文以空格分词：GPUI 把韩文字母当作可以随处断开的字，这里在韩文字母与紧挨着的非空白字符之间不断开，只在空格
//!   处换行（与 CSS 的 `word-break: keep-all` 相同，[`keeps_together`]）；一个词比整行还宽时才从词中间断开。
//!
//! 只在文字里有这些标点、假名或韩文时才自己排版；其余（英文、截断显示的、不换行的）原样交给 GPUI 的文字元素，
//! 结果完全相同。文字本身不变（不插入任何字符），复制、搜索不受影响；终端自己排版，不经过这里。

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::{
    App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, IndentAdjustment,
    InspectorElementId, IntoElement, LayoutId, LineFragment, LineWrapper, Pixels, ShapedLine,
    SharedString, Size, TextAlign, TextLayout, TextRun, WhiteSpace, Window,
};

/// 照避头尾规则换行的文字（用法与直接放字符串相同：`div().child(prose(t!("…")))`；gpui-kit 的对话框说明
/// `description(...)`、表单字段的说明也可以传它）。
pub(crate) fn prose(text: impl Into<SharedString>) -> Prose {
    Prose {
        text: text.into(),
        laid: Rc::default(),
    }
}

pub(crate) struct Prose {
    text: SharedString,
    laid: Rc<RefCell<Option<Laid>>>,
}

/// 自己排好的行（最后一次量尺寸时的结果）。
struct Laid {
    lines: Vec<ShapedLine>,
    line_height: Pixels,
    wrap_width: Option<Pixels>,
    align: TextAlign,
    size: Size<Pixels>,
}

/// 这次由谁排版。
pub(crate) enum ProseLayout {
    /// GPUI 的文字元素（没有中文标点、截断显示、限制行数时）
    Gpui(TextLayout),
    Own,
}

/// 表单字段的说明（gpui-kit 的 `field().description(...)`）也用避头尾的文字。
impl From<Prose> for gpui_kit::component::form::FieldBuilder {
    fn from(prose: Prose) -> Self {
        let text = prose.text;
        Self::Element(Rc::new(move |_, _| {
            self::prose(text.clone()).into_any_element()
        }))
    }
}

/// 会换行的复选框标签（较长、放在窄的对话框中）：gpui-kit 的 `label(...)` 直接交给 GPUI 排版，会把韩文的词拆到两行，
/// 用 `prose_label(...)` 代替，换行与 [`prose`] 相同，读屏的名称不变。
pub(crate) trait ProseLabel {
    fn prose_label(self, text: impl Into<SharedString>) -> Self;
}

impl ProseLabel for gpui_kit::component::checkbox::Checkbox {
    fn prose_label(self, text: impl Into<SharedString>) -> Self {
        use gpui_kit::ParentElement as _;
        let text = text.into();
        self.accessibility_label(text.clone()).child(prose(text))
    }
}

impl IntoElement for Prose {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Prose {
    type RequestLayoutState = ProseLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let own = style.text_overflow.is_none()
            && style.line_clamp.is_none()
            && style.white_space == WhiteSpace::Normal
            && applies(&self.text);
        if !own {
            let (layout_id, layout) = self.text.request_layout(id, inspector_id, window, cx);
            return (layout_id, ProseLayout::Gpui(layout));
        }
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        let run = style.to_run(0);
        let font = style.font();
        let align = style.text_align;
        let text = self.text.clone();
        let laid = self.laid.clone();
        let layout_id = window.request_measured_layout(
            Default::default(),
            move |known, available, window, cx| {
                let wrap_width = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                // 与 GPUI 的文字元素相同：量过的尺寸在宽度相同或不限宽度的再次测量时沿用（flex 布局的结果一致）
                if let Some(laid) = laid.borrow().as_ref()
                    && (wrap_width.is_none() || laid.wrap_width == wrap_width)
                {
                    return laid.size;
                }
                let mut wrapper = cx.text_system().line_wrapper(font.clone(), font_size);
                let mut lines = Vec::new();
                for paragraph in text.split('\n') {
                    let ranges = match wrap_width {
                        Some(width) => wrap(paragraph, &mut wrapper, width),
                        None => std::iter::once(0..paragraph.len()).collect(),
                    };
                    for range in ranges {
                        // 行尾的空格不画（换行处留在上一行的空格不占宽度）
                        let text = paragraph[range].trim_end_matches(' ');
                        let line = SharedString::from(text.to_string());
                        let runs = [TextRun {
                            len: line.len(),
                            ..run.clone()
                        }];
                        lines.push(
                            window
                                .text_system()
                                .shape_line(line, font_size, &runs, None),
                        );
                    }
                }
                let mut size = Size::<Pixels>::default();
                for line in &lines {
                    size.width = size.width.max(line.width).ceil();
                    size.height += line_height;
                }
                *laid.borrow_mut() = Some(Laid {
                    lines,
                    line_height,
                    wrap_width,
                    align,
                    size,
                });
                size
            },
        );
        (layout_id, ProseLayout::Own)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let ProseLayout::Gpui(layout) = layout {
            self.text
                .prepaint(id, inspector_id, bounds, layout, window, cx);
        }
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let ProseLayout::Gpui(layout) = layout {
            self.text
                .paint(id, inspector_id, bounds, layout, &mut (), window, cx);
            return;
        }
        let laid = self.laid.borrow();
        let Some(laid) = laid.as_ref() else {
            return;
        };
        let mut origin = bounds.origin;
        for line in &laid.lines {
            if let Err(e) = line.paint(
                origin,
                laid.line_height,
                laid.align,
                Some(bounds.size.width),
                window,
                cx,
            ) {
                tracing::warn!("画文字失败：{e}");
            }
            origin.y += laid.line_height;
        }
    }
}

/// 一段（没有换行符）按 `width` 换行：GPUI 找出的每个断点按避头尾规则往前挪，挪过之后从新的行首重新找下一个断点。
/// 断点处的空格算在上一行的行尾。返回各行在 `paragraph` 中的范围。
fn wrap(paragraph: &str, wrapper: &mut LineWrapper, width: Pixels) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < paragraph.len() {
        let rest = &paragraph[start..];
        let fragments = [LineFragment::text(rest)];
        let next = wrapper
            .wrap_line(&fragments, width, IndentAdjustment::NoIndent)
            .next()
            .map(|boundary| boundary.ix);
        let Some(ix) = next.filter(|ix| *ix > 0 && *ix < rest.len()) else {
            lines.push(start..paragraph.len());
            return lines;
        };
        let mut ix = adjust_break(rest, ix);
        // 断点处的空格留在上一行的行尾（排版时去掉，不占宽度），下一行不以空格开头
        ix = rest.len() - rest[ix..].trim_start_matches(' ').len();
        lines.push(start..start + ix);
        start += ix;
    }
    if lines.is_empty() {
        lines.push(0..0);
    }
    lines
}

/// 行首禁则：不放在行首的标点（句末、句中点号，右括号、右引号，省略号、间隔号等），以及日文的小写假名、长音与
/// 叠字符号（JIS X 4051 的行头禁则）。
pub(crate) fn no_line_start(c: char) -> bool {
    small_kana(c)
        || matches!(
            c,
            'ー' | '々'
                | '〻'
                | 'ゝ'
                | 'ゞ'
                | 'ヽ'
                | 'ヾ'
                | '゛'
                | '゜'
                | '‐'
                | '゠'
                | '〜'
                | '〟'
                | '｡'
                | '､'
                | '｣'
                | '?'
                | '!'
                | '‼'
                | '⁇'
                | '⁈'
                | '⁉'
        )
        || matches!(
            c,
            '。' | '，'
                | '、'
                | '；'
                | '：'
                | '？'
                | '！'
                | '．'
                | '）'
                | '］'
                | '｝'
                | '」'
                | '』'
                | '〕'
                | '】'
                | '》'
                | '〉'
                | '〗'
                | '〙'
                | '〛'
                | '”'
                | '’'
                | '…'
                | '‥'
                | '·'
                | '・'
                | '～'
                | '％'
                | ')'
                | ']'
                | '}'
        )
}

/// 日文的小写假名（ぁ、っ、ゃ、ァ、ッ、ㇰ 等）：促音、拗音不放在行首。
fn small_kana(c: char) -> bool {
    matches!(
        c,
        'ぁ' | 'ぃ'
            | 'ぅ'
            | 'ぇ'
            | 'ぉ'
            | 'っ'
            | 'ゃ'
            | 'ゅ'
            | 'ょ'
            | 'ゎ'
            | 'ゕ'
            | 'ゖ'
            | 'ァ'
            | 'ィ'
            | 'ゥ'
            | 'ェ'
            | 'ォ'
            | 'ッ'
            | 'ャ'
            | 'ュ'
            | 'ョ'
            | 'ヮ'
            | 'ヵ'
            | 'ヶ'
            | '\u{31F0}'..='\u{31FF}'
    )
}

/// 韩文字母（音节与字母）。
fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
}

/// 韩文以空格分词：韩文字母与紧挨着的非空白字符（另一个韩文字母、英文、数字、标点）之间不断开。
pub(crate) fn keeps_together(before: char, after: char) -> bool {
    (is_hangul(before) && !after.is_whitespace()) || (is_hangul(after) && !before.is_whitespace())
}

/// 行尾禁则：不留在行尾的标点（左括号、左引号）。
pub(crate) fn no_line_end(c: char) -> bool {
    matches!(
        c,
        '（' | '［'
            | '｛'
            | '「'
            | '『'
            | '〔'
            | '【'
            | '《'
            | '〈'
            | '〖'
            | '〘'
            | '〚'
            | '“'
            | '‘'
            | '('
            | '['
            | '{'
            | '〝'
            | '｢'
    )
}

/// 不能拆到两行的一对：破折号“——”“――”、省略号“……”“‥‥”。
fn inseparable(before: char, after: char) -> bool {
    before == after && matches!(before, '—' | '―' | '…' | '‥' | '⋯')
}

/// 文字里有没有要自己排版的字：避头尾的标点、日文的小写假名与长音、韩文（只看非 ASCII 的，纯英文的文字交给
/// GPUI）。
pub(crate) fn applies(text: &str) -> bool {
    text.chars().any(|c| {
        !c.is_ascii()
            && (no_line_start(c) || no_line_end(c) || matches!(c, '—' | '―') || is_hangul(c))
    })
}

/// 把在 `ix`（下一行开头的字节位置）断行按避头尾规则调整：下一行以行首禁则标点开头、这一行以行尾禁则标点结尾、
/// 断在不可分的一对中间，或者断在韩文的词中间时，断点往前挪一个字（挪到英文单词中间时挪到单词前），直到都不违反。
/// 挪到行首（这一行一个字也不剩）时：韩文的词比整行还宽，不再按词、只按避头尾再挪一次；还是挪到行首就放弃，用原来
/// 的断点。
pub(crate) fn adjust_break(line: &str, ix: usize) -> usize {
    find_break(line, ix, true)
        .or_else(|| find_break(line, ix, false))
        .unwrap_or(ix)
}

/// 从 `ix` 往前找第一个可以断开的位置（`by_word`：韩文按词，不在词中间断）；挪到行首时为 `None`。
fn find_break(line: &str, ix: usize, by_word: bool) -> Option<usize> {
    let mut at = ix;
    loop {
        // 前面只有空白也算行首（这一行什么也不剩）
        if line[..at].trim_start().is_empty() {
            return None;
        }
        let before = line[..at].chars().next_back()?;
        let after = line[at..].chars().next();
        // 往前挪时不把英文单词、数字从中间拆开（GPUI 本来就不在单词中间断）
        let in_word = |a: char| before.is_ascii_alphanumeric() && a.is_ascii_alphanumeric();
        let bad = after.is_some_and(no_line_start)
            || no_line_end(before)
            || after.is_some_and(|after| {
                inseparable(before, after) || (by_word && keeps_together(before, after))
            })
            || (at != ix && after.is_some_and(in_word));
        if !bad {
            return Some(at);
        }
        at -= before.len_utf8();
        if at == 0 {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext, div, font, px};

    use super::*;

    /// 断在第 `chars` 个字之后（按字数，不是字节）调整后的结果，用“|”标出断点。
    fn adjusted(line: &str, chars: usize) -> String {
        let ix = line
            .char_indices()
            .nth(chars)
            .map_or(line.len(), |(ix, _)| ix);
        let at = adjust_break(line, ix);
        format!("{}|{}", &line[..at], &line[at..])
    }

    #[test]
    fn breaks_move_before_the_previous_character() {
        // 句号不放在行首：带上前一个字
        assert_eq!(adjusted("看不到處理程序。", 7), "看不到處理程|序。");
        assert_eq!(adjusted("看不到进程。", 5), "看不到进|程。");
        assert_eq!(adjusted("甲乙丙丁，戊己", 4), "甲乙丙|丁，戊己");
        // 连续的标点一起带走
        assert_eq!(adjusted("他说“好的。”然后", 6), "他说“好|的。”然后");
        assert_eq!(adjusted("真的吗？！", 4), "真的|吗？！");
        // 左括号、左引号不留在行尾
        assert_eq!(adjusted("甲乙丙「丁戊」", 4), "甲乙丙|「丁戊」");
        assert_eq!(adjusted("见设置（通用）", 4), "见设置|（通用）");
        // 破折号、省略号不拆开
        assert_eq!(adjusted("他说——好", 3), "他说|——好");
        assert_eq!(adjusted("等等……吧", 3), "等|等……吧");
        // 不违反规则的断点不动
        assert_eq!(adjusted("看不到處理程序。", 4), "看不到處|理程序。");
        // 混排英文：不把单词拆开；已有空格处照常
        assert_eq!(adjusted("没有 ssh。", 6), "没有 |ssh。");
        assert_eq!(
            adjusted("读取 /proc/net：看不到", 12),
            "读取 /proc/|net：看不到"
        );
        assert_eq!(adjusted("Hello world", 6), "Hello |world");
        // 全是禁则标点、挪到行首：放弃，用原来的断点
        assert_eq!(adjusted("。。。。", 2), "。。|。。");
        assert_eq!(adjusted("甲。", 1), "甲|。");
    }

    #[test]
    fn japanese_small_kana_and_long_vowels_stay_off_line_starts() {
        // 小写假名（促音、拗音）、长音、叠字符号不放在行首
        assert_eq!(adjusted("少しちょっと", 4), "少し|ちょっと");
        assert_eq!(adjusted("接続サーバー", 3), "接続|サーバー");
        assert_eq!(adjusted("接続サーバー", 5), "接続サー|バー");
        assert_eq!(adjusted("新しいセッション", 4), "新しい|セッション");
        assert_eq!(adjusted("新しいセッション", 6), "新しいセッ|ション");
        assert_eq!(adjusted("前日々の", 2), "前|日々の");
        // 「」、句号、破折号同中文
        assert_eq!(adjusted("設定「一般」を開く", 3), "設定|「一般」を開く");
        assert_eq!(adjusted("接続しました。", 6), "接続しまし|た。");
        assert_eq!(adjusted("待って――はい", 4), "待って|――はい");
        // 不违反规则的断点不动：片假名词可以从中间断开
        assert_eq!(adjusted("接続サーバー", 4), "接続サー|バー");
        // 挪到行首：放弃，用原来的断点
        assert_eq!(adjusted("ファイル", 1), "フ|ァイル");
    }

    #[test]
    fn korean_breaks_only_between_words() {
        // 断在词中间时挪到前一个空格之后
        assert_eq!(
            adjusted("호스트를 삭제하시겠습니까?", 7),
            "호스트를 |삭제하시겠습니까?"
        );
        // 助词、标点跟着前面的英文、数字、插值
        assert_eq!(adjusted("새 Termlet을 엽니다", 9), "새 |Termlet을 엽니다");
        assert_eq!(adjusted("파일 3개", 4), "파일 |3개");
        assert_eq!(adjusted("다시 시도할까요?", 8), "다시 |시도할까요?");
        // 在空格处断开的不动
        assert_eq!(adjusted("호스트를 삭제", 5), "호스트를 |삭제");
        // 一个词比整行还宽（挪到行首）：从词中间断开，仍按避头尾
        assert_eq!(adjusted("호스트를 삭제", 2), "호스|트를 삭제");
        assert_eq!(adjusted("설정(일반)", 2), "설정|(일반)");
        assert_eq!(adjusted("할까요?", 3), "할까|요?");
    }

    #[test]
    fn only_cjk_punctuation_kana_and_hangul_apply() {
        assert!(applies("看不到處理程序。"));
        assert!(applies("见「设置」"));
        assert!(applies("他说——好"));
        assert!(applies("ちょっと待つ"), "日文的小写假名");
        assert!(applies("サーバー"), "日文的长音");
        assert!(applies("호스트 삭제"), "韩文按词换行");
        assert!(!applies("Hello (world)."), "纯英文交给 GPUI");
        assert!(!applies("Done? Yes!"), "英文的问号、叹号交给 GPUI");
        assert!(!applies("没有标点的中文"));
        assert!(!applies("新しい接続"));
    }

    /// 按 GPUI 的换行（测试的文字系统每个字宽 0.6em）在各种宽度下换行：每行不以行首禁则标点开头、不以行尾禁则
    /// 标点结尾，不比宽度宽，拼回去还是原文。
    #[gpui_kit::test]
    fn no_punctuation_at_line_starts_at_any_width(cx: &mut TestAppContext) {
        let samples = [
            "這台主機沒有 ss，改為讀取 /proc/net：看不到處理程序。",
            "这台主机没有 ss，读取 /proc/net：看不到进程。",
            "点“+”后按下新的按键。快捷键由 Termlet 处理，不再发给远端；需要带 ⌘ 或 Control（F1–F24 除外）。",
            "按「+」後按下新的按鍵……然後——儲存。",
            "Only hidden items here (3). Press Ctrl+Alt+H to show them.",
            "このホストには ss がないため、/proc/net を読み取りました。プロセスは表示できません。",
            "「設定」→「一般」でショートカットキーを変更してください。ちょっと待ってください。",
            "이 호스트에는 ss가 없어 /proc/net을 읽었습니다. 프로세스를 표시할 수 없습니다.",
            "'web-01' 호스트를 삭제하시겠습니까? Termlet을 다시 시작하세요.",
        ];
        let font_size = px(10.);
        let char_width = 6.;
        cx.update(|cx| {
            let mut wrapper = cx
                .text_system()
                .line_wrapper(font(".SystemUIFont"), font_size);
            for text in samples {
                for chars in 3..=text.chars().count() {
                    let width = px(char_width * chars as f32 + 0.01);
                    let lines: Vec<&str> = wrap(text, &mut wrapper, width)
                        .into_iter()
                        .map(|r| &text[r])
                        .collect();
                    assert_eq!(lines.concat(), text);
                    for (i, line) in lines.iter().enumerate() {
                        // 行尾的空格不占宽度
                        let count = line.trim_end().chars().count();
                        assert!(count <= chars, "{chars} 字宽：第 {i} 行太长 {lines:?}");
                        if i > 0 {
                            let first = line.chars().next().unwrap();
                            assert!(!no_line_start(first), "{chars} 字宽：{lines:?}");
                        }
                        if i + 1 < lines.len() {
                            let Some(last) = line.trim_end().chars().next_back() else {
                                panic!("{chars} 字宽：第 {i} 行只有空白 {lines:?}");
                            };
                            assert!(!no_line_end(last), "{chars} 字宽：{lines:?}");
                            // 韩文的词不拆开，除非这一行只有这一个词（词比整行还宽）
                            let end = line.chars().next_back().unwrap();
                            let next = lines[i + 1].chars().next().unwrap();
                            if keeps_together(end, next) {
                                assert!(
                                    !line.trim_start().contains(' '),
                                    "{chars} 字宽：{lines:?}"
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    struct Paragraph(SharedString, Pixels);

    impl gpui_kit::Render for Paragraph {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            use gpui_kit::{
                InteractiveElement as _, ParentElement as _, Styled as _, TestSupportExt as _,
            };
            // 外层按内容的高度放（默认会撑满窗口）
            div().flex().flex_col().items_start().child(
                div()
                    .id("paragraph")
                    .test_support()
                    .w(self.1)
                    .text_size(px(10.))
                    .line_height(px(14.))
                    .child(prose(self.0.clone())),
            )
        }
    }

    /// 放进界面：有中文标点的按自己排的行数，英文与直接放字符串的高度相同。
    #[gpui_kit::test]
    fn renders_as_many_lines_as_wrapped(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        cx.update(gpui_kit::init);
        let height = |text: &'static str, width: f32, cx: &mut TestAppContext| {
            let window = cx.add_window(|_, _| Paragraph(text.into(), px(width)));
            cx.update_window(window.into(), |_, window, cx| {
                window.render_frame(cx);
                window.find("paragraph").bounds().size.height
            })
            .unwrap()
        };
        // 7 个字宽：“看不到處理程”“序。”两行（GPUI 原来是“看不到處理程序”“。”，也是两行，但句号在行首）
        assert_eq!(height("看不到處理程序。", 6. * 7. + 0.5, cx), px(28.));
        assert_eq!(height("看不到處理程序。", 6. * 8. + 0.5, cx), px(14.));
        assert_eq!(height("abc def ghi", 6. * 7. + 0.5, cx), px(28.));
        // GPUI 排成“甲乙丙丁戊己庚”“。辛壬癸子丑寅”两行；带走“庚”后要三行
        assert_eq!(
            height("甲乙丙丁戊己庚。辛壬癸子丑寅", 6. * 7. + 0.5, cx),
            px(42.)
        );
    }
}
