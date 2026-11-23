//! HTML to plain text (mails/plain-text.ts).
//!
//! Plain-text rendering of mail HTML: block elements start new lines, text
//! wraps at 80 columns, headings are uppercased, list items get ` * ` or
//! `1. ` prefixes, quotes get `> `, links keep only their text, and images,
//! scripts, styles and comments are dropped. With `urls`, links and images
//! also show their target in brackets.
//!
//! Markup is read the lenient way mail clients write it, with no attempt at
//! full HTML5 tree construction: a few elements close their open siblings
//! (`<p>`, `<li>`, `<td>`...), void elements never take children, and a
//! stray end tag closes back to its matching start tag or is ignored.
//!
//! The TypeScript renderer recurses over the element tree; this one keeps
//! its own stack, so nesting depth costs memory, not call stack.

use std::collections::LinkedList;

use super::entities::{decode_attribute_entities, decode_entities};
use crate::mime::charset::jsstr;

/// Why the TypeScript renderer throws on an input, with the text rendered
/// past the failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct PlainTextError {
    pub reason: PlainTextFailure,
    /// The rendering with the failing construct read leniently: the tag
    /// closes nothing, the list marker is the plain number.
    pub rendered: String,
}

/// The inputs the TypeScript renderer throws a `TypeError` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlainTextFailure {
    /// A `<constructor>` tag inside an open element: the lookup of which
    /// elements it closes finds `Object.prototype.constructor`.
    #[error("implied.has is not a function. (In 'implied.has(this.open[this.open.length - 1])', 'implied.has' is undefined)")]
    ConstructorTag,
    /// A Roman-numeral list marker of 10000 or more.
    #[error("undefined is not an object (evaluating 'ROMAN_ONES[i].repeat')")]
    RomanNumeral,
}

type NodeId = usize;

#[derive(Default)]
struct Element {
    name: String,
    /// The attributes the renderer reads, first occurrence of each.
    attribs: Vec<(&'static str, String)>,
    children: Vec<NodeId>,
    parent: Option<NodeId>,
}

impl Element {
    fn attrib(&self, name: &str) -> Option<&str> {
        self.attribs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

enum Node {
    Element(Element),
    Text(String),
    /// Comments and declarations: they hold no text but still count as list
    /// children.
    Other,
}

const READ_ATTRIBUTES: [&str; 5] = ["alt", "src", "href", "start", "type"];

const P: &[&str] = &["p"];
const FORM: &[&str] = &[
    "input", "option", "optgroup", "select", "button", "datalist", "textarea",
];

/// Opening one of these closes the listed elements when they are the
/// innermost open one.
fn open_implies_close(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "tr" => &["tr", "th", "td"],
        "th" => &["th"],
        "td" => &["thead", "th", "td"],
        "body" => &["head", "link", "script"],
        "li" => &["li"],
        "option" => &["option"],
        "optgroup" => &["optgroup", "option"],
        "dd" | "dt" => &["dt", "dd"],
        "rt" | "rp" => &["rt", "rp"],
        "tbody" | "tfoot" => &["thead", "tbody"],
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "address" | "article" | "aside" | "blockquote" | "details"
        | "div" | "dl" | "fieldset" | "figcaption" | "figure" | "footer" | "form" | "header" | "hr" | "main"
        | "nav" | "ol" | "pre" | "section" | "table" | "ul" | "p" => P,
        "select" | "input" | "output" | "button" | "datalist" | "textarea" => FORM,
        _ => return None,
    })
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "br"
            | "col"
            | "command"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "isindex"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Inside these, `<x/>` closes itself.
fn is_foreign(name: &str) -> bool {
    matches!(name, "math" | "svg")
}

/// `foreignObject` is spelled with its capital here as there, so a
/// lowercased tag name never matches it.
fn is_html_integration(name: &str) -> bool {
    matches!(
        name,
        "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml" | "foreignObject" | "desc" | "title"
    )
}

/// Elements whose content is raw text up to their end tag.
fn raw_text_name(name: &str) -> Option<&'static str> {
    ["script", "style", "title"].into_iter().find(|raw| *raw == name)
}

/// Content kept out of the text entirely.
fn is_skipped(name: &str) -> bool {
    matches!(name, "script" | "style")
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\n' | b'\t' | 0x0c | b'\r')
}

/// Builds the element tree from tokenizer events.
struct TreeBuilder {
    nodes: Vec<Node>,
    current: NodeId,
    open: Vec<String>,
    foreign: Vec<bool>,
    last_text: Option<NodeId>,
    tag_name: String,
    attribs: Vec<(&'static str, String)>,
    failure: Option<PlainTextFailure>,
}

const ROOT: NodeId = 0;

impl TreeBuilder {
    fn new() -> TreeBuilder {
        TreeBuilder {
            nodes: vec![Node::Element(Element::default())],
            current: ROOT,
            open: Vec::new(),
            foreign: Vec::new(),
            last_text: None,
            tag_name: String::new(),
            attribs: Vec::new(),
            failure: None,
        }
    }

    fn element(&mut self, id: NodeId) -> &mut Element {
        match &mut self.nodes[id] {
            Node::Element(element) => element,
            _ => unreachable!("only elements are parents"),
        }
    }

    fn append(&mut self, node: Node) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(node);
        let parent = self.current;
        self.element(parent).children.push(id);
        id
    }

    fn text(&mut self, data: &str) {
        if let Some(Node::Text(text)) = self.last_text.map(|id| &mut self.nodes[id]) {
            text.push_str(data);
            return;
        }
        self.last_text = Some(self.append(Node::Text(data.to_string())));
    }

    /// A comment, CDATA section, declaration or processing instruction.
    fn other(&mut self) {
        self.append(Node::Other);
        self.last_text = None;
    }

    fn open_tag_name(&mut self, raw: &str) {
        let name = raw.to_lowercase();
        if name == "constructor" && !self.open.is_empty() {
            self.failure.get_or_insert(PlainTextFailure::ConstructorTag);
        }
        if let Some(implied) = open_implies_close(&name) {
            while self
                .open
                .last()
                .is_some_and(|innermost| implied.contains(&innermost.as_str()))
            {
                self.open.pop();
                self.pop_element();
            }
        }
        if !is_void(&name) {
            if is_foreign(&name) {
                self.foreign.push(true);
            } else if is_html_integration(&name) {
                self.foreign.push(false);
            }
            self.open.push(name.clone());
        }
        self.tag_name = name;
        self.attribs.clear();
    }

    fn attribute(&mut self, name: &str, value: &str) {
        let name = name.to_lowercase();
        let Some(key) = READ_ATTRIBUTES.into_iter().find(|key| *key == name) else {
            return;
        };
        if !self.attribs.iter().any(|(seen, _)| *seen == key) {
            self.attribs.push((key, value.to_string()));
        }
    }

    fn open_tag_end(&mut self) {
        let name = std::mem::take(&mut self.tag_name);
        let void = is_void(&name);
        let element = Element {
            name,
            attribs: std::mem::take(&mut self.attribs),
            children: Vec::new(),
            parent: Some(self.current),
        };
        self.current = self.append(Node::Element(element));
        self.last_text = None;
        if void {
            self.pop_element();
        }
    }

    fn self_closing_tag(&mut self) {
        if self.foreign.last() == Some(&true) {
            self.close_current_tag();
        } else {
            self.open_tag_end();
        }
    }

    fn close_current_tag(&mut self) {
        let name = self.tag_name.clone();
        self.open_tag_end();
        if self.open.last() == Some(&name) {
            self.pop_element();
            self.open.pop();
        }
    }

    fn pop_element(&mut self) {
        let current = self.current;
        self.current = self.element(current).parent.unwrap_or(ROOT);
        self.last_text = None;
    }

    fn close_tag(&mut self, raw: &str) {
        let name = raw.to_lowercase();
        if is_foreign(&name) || is_html_integration(&name) {
            self.foreign.pop();
        }
        if !self.open.is_empty() && !is_void(&name) {
            if let Some(index) = self.open.iter().rposition(|open| *open == name) {
                for _ in index..self.open.len() {
                    self.pop_element();
                }
                self.open.truncate(index);
            } else if name == "p" {
                self.open_tag_name(&name);
                self.close_current_tag();
            }
        } else if name == "br" || name == "p" {
            self.open_tag_name(&name);
            self.close_current_tag();
        }
    }

    fn end(&mut self) {
        while self.open.pop().is_some() {
            self.pop_element();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Text,
    BeforeTagName,
    InTagName,
    BeforeClosingTagName,
    InClosingTagName,
    AfterClosingTagName,
    BeforeAttributeName,
    InSelfClosingTag,
    InAttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    InAttributeValueDq,
    InAttributeValueSq,
    InAttributeValueNq,
    BeforeDeclaration,
    InDeclaration,
    InProcessingInstruction,
    BeforeComment,
    InComment,
    InSpecialComment,
    AfterComment1,
    AfterComment2,
    BeforeCdata,
    InCdata,
    AfterCdata1,
    AfterCdata2,
}

/// Where the character after `at` starts.
fn next_char(html: &str, at: usize) -> usize {
    match html[at..].chars().next() {
        Some(c) => at + c.len_utf8(),
        None => html.len(),
    }
}

/// Reads a possible end tag for raw text at `index` (just after `</`). On a
/// match, the answer is the end of the name; otherwise it is where text
/// reading resumes, which depends on how far the name matched.
fn raw_text_end(html: &str, index: usize, name: &str) -> Result<usize, usize> {
    let bytes = html.as_bytes();
    let matched = name
        .bytes()
        .zip(&bytes[index..])
        .take_while(|(expected, found)| found.to_ascii_lowercase() == *expected)
        .count();
    let after = index + matched;
    if matched == name.len() {
        if bytes.get(after).is_some_and(|&c| c == b'>' || is_space(c)) {
            return Ok(after);
        }
        return Err(next_char(html, after));
    }
    // A wrong second letter of "script"/"style" is consumed; other mismatches are reread.
    if matched == 1 && name != "title" {
        return Err(next_char(html, after));
    }
    Err(after)
}

/// One pass over the input. Every character the states look for is ASCII,
/// so the bytes of the UTF-8 text are read one by one.
fn tokenize(html: &str, tree: &mut TreeBuilder) {
    let bytes = html.as_bytes();
    let mut state = State::Text;
    // The start of the section being read; `None` between sections.
    let mut section_start: Option<usize> = Some(0);
    let mut raw_text: Option<&'static str> = None;
    let mut attrib_name = "";
    let mut cdata_matched = 0;
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];
        // Only read where `i` is at one of the ASCII characters that end a section.
        let section = |i: usize| -> &str { &html[section_start.unwrap_or(i)..i] };
        // Whether the next round reads the character after this one.
        let mut advance = true;
        match state {
            State::Text => {
                if c == b'<' {
                    if !section(i).is_empty() {
                        tree.text(section(i));
                    }
                    state = State::BeforeTagName;
                    section_start = Some(i);
                }
            }
            State::BeforeTagName => {
                if c == b'/' {
                    state = State::BeforeClosingTagName;
                } else if c == b'<' {
                    tree.text(section(i));
                    section_start = Some(i);
                } else if c == b'>' || raw_text.is_some() || is_space(c) {
                    state = State::Text;
                } else if c == b'!' {
                    state = State::BeforeDeclaration;
                    section_start = Some(i + 1);
                } else if c == b'?' {
                    state = State::InProcessingInstruction;
                    section_start = Some(i + 1);
                } else if !c.is_ascii_alphabetic() {
                    state = State::Text;
                } else {
                    state = State::InTagName;
                    section_start = Some(i);
                }
            }
            State::InTagName => {
                if c == b'/' || c == b'>' || is_space(c) {
                    let name = section(i);
                    tree.open_tag_name(name);
                    if let Some(raw) = raw_text_name(&name.to_lowercase()) {
                        raw_text = Some(raw);
                    }
                    section_start = None;
                    state = State::BeforeAttributeName;
                    advance = false;
                }
            }
            State::BeforeClosingTagName => {
                if is_space(c) {
                    // Ignored.
                } else if c == b'>' {
                    state = State::Text;
                } else if let Some(name) = raw_text {
                    // Inside raw text only the element's own end tag counts.
                    match raw_text_end(html, i, name) {
                        Ok(name_end) => {
                            raw_text = None;
                            state = State::InClosingTagName;
                            section_start = Some(i);
                            i = name_end;
                        }
                        Err(resume) => {
                            state = State::Text;
                            i = resume;
                        }
                    }
                    advance = false;
                } else if !c.is_ascii_alphabetic() {
                    state = State::InSpecialComment;
                    section_start = Some(i);
                } else {
                    state = State::InClosingTagName;
                    section_start = Some(i);
                }
            }
            State::InClosingTagName => {
                if c == b'>' || is_space(c) {
                    tree.close_tag(section(i));
                    section_start = None;
                    state = State::AfterClosingTagName;
                    advance = false;
                }
            }
            State::AfterClosingTagName => {
                if c == b'>' {
                    state = State::Text;
                    section_start = Some(i + 1);
                }
            }
            State::BeforeAttributeName => {
                if c == b'>' {
                    tree.open_tag_end();
                    state = State::Text;
                    section_start = Some(i + 1);
                } else if c == b'/' {
                    state = State::InSelfClosingTag;
                } else if !is_space(c) {
                    state = State::InAttributeName;
                    section_start = Some(i);
                }
            }
            State::InSelfClosingTag => {
                if c == b'>' {
                    tree.self_closing_tag();
                    state = State::Text;
                    section_start = Some(i + 1);
                    raw_text = None;
                } else if !is_space(c) {
                    state = State::BeforeAttributeName;
                    advance = false;
                }
            }
            State::InAttributeName => {
                if c == b'=' || c == b'/' || c == b'>' || is_space(c) {
                    attrib_name = section(i);
                    section_start = None;
                    state = State::AfterAttributeName;
                    advance = false;
                }
            }
            State::AfterAttributeName => {
                if c == b'=' {
                    state = State::BeforeAttributeValue;
                } else if c == b'/' || c == b'>' {
                    tree.attribute(attrib_name, "");
                    state = State::BeforeAttributeName;
                    advance = false;
                } else if !is_space(c) {
                    tree.attribute(attrib_name, "");
                    state = State::InAttributeName;
                    section_start = Some(i);
                }
            }
            State::BeforeAttributeValue => {
                if c == b'"' {
                    state = State::InAttributeValueDq;
                    section_start = Some(i + 1);
                } else if c == b'\'' {
                    state = State::InAttributeValueSq;
                    section_start = Some(i + 1);
                } else if !is_space(c) {
                    state = State::InAttributeValueNq;
                    section_start = Some(i);
                    advance = false;
                }
            }
            State::InAttributeValueDq | State::InAttributeValueSq => {
                let quote = if state == State::InAttributeValueDq {
                    b'"'
                } else {
                    b'\''
                };
                if c == quote {
                    tree.attribute(attrib_name, section(i));
                    section_start = None;
                    state = State::BeforeAttributeName;
                }
            }
            State::InAttributeValueNq => {
                if is_space(c) || c == b'>' {
                    tree.attribute(attrib_name, section(i));
                    section_start = None;
                    state = State::BeforeAttributeName;
                    advance = false;
                }
            }
            State::BeforeDeclaration => {
                if c == b'[' {
                    state = State::BeforeCdata;
                    cdata_matched = 0;
                } else {
                    state = if c == b'-' {
                        State::BeforeComment
                    } else {
                        State::InDeclaration
                    };
                }
            }
            State::BeforeCdata => {
                if cdata_matched < 5 && c.to_ascii_uppercase() == b"CDATA"[cdata_matched] {
                    cdata_matched += 1;
                } else if cdata_matched == 5 && c == b'[' {
                    state = State::InCdata;
                    section_start = Some(i + 1);
                } else {
                    state = State::InDeclaration;
                    advance = false;
                }
            }
            State::InDeclaration | State::InProcessingInstruction | State::InSpecialComment => {
                if c == b'>' {
                    tree.other();
                    state = State::Text;
                    section_start = Some(i + 1);
                }
            }
            State::BeforeComment => {
                if c == b'-' {
                    state = State::InComment;
                    section_start = Some(i + 1);
                } else {
                    state = State::InDeclaration;
                }
            }
            State::InComment => {
                if c == b'-' {
                    state = State::AfterComment1;
                }
            }
            State::AfterComment1 => {
                state = if c == b'-' {
                    State::AfterComment2
                } else {
                    State::InComment
                };
            }
            State::AfterComment2 => {
                if c == b'>' {
                    tree.other();
                    state = State::Text;
                    section_start = Some(i + 1);
                } else if c != b'-' {
                    state = State::InComment;
                }
            }
            State::InCdata => {
                if c == b']' {
                    state = State::AfterCdata1;
                }
            }
            State::AfterCdata1 => {
                state = if c == b']' { State::AfterCdata2 } else { State::InCdata };
            }
            State::AfterCdata2 => {
                if c == b'>' {
                    tree.other();
                    state = State::Text;
                    section_start = Some(i + 1);
                } else if c != b']' {
                    state = State::InCdata;
                }
            }
        }
        if advance {
            i += 1;
        }
    }

    if let Some(start) = section_start.filter(|&start| start < html.len()) {
        match state {
            // An unfinished comment still counts as one.
            State::InComment
            | State::AfterComment1
            | State::AfterComment2
            | State::InCdata
            | State::AfterCdata1
            | State::AfterCdata2 => tree.other(),
            // Trailing input that is part of an unfinished tag is dropped.
            State::InTagName
            | State::BeforeAttributeName
            | State::BeforeAttributeValue
            | State::AfterAttributeName
            | State::InAttributeName
            | State::InAttributeValueSq
            | State::InAttributeValueDq
            | State::InAttributeValueNq
            | State::InClosingTagName => {}
            _ => tree.text(&html[start..]),
        }
    }
    tree.end();
}

const WORDWRAP: usize = 80;

/// HTML whitespace, plus the zero-width space.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n' | '\x0c' | '\u{200b}')
}

/// Collects words into lines no longer than `max_line_length`.
struct LineBuilder {
    lines: Vec<Vec<String>>,
    words: Vec<String>,
    max_line_length: usize,
    available: isize,
    stashed_space: bool,
    word_break_opportunity: bool,
}

impl LineBuilder {
    fn new(max_line_length: usize) -> LineBuilder {
        LineBuilder {
            lines: Vec::new(),
            words: Vec::new(),
            max_line_length,
            available: max_line_length as isize,
            stashed_space: false,
            word_break_opportunity: false,
        }
    }

    fn push_word(&mut self, word: String) {
        if self.available <= 0 {
            self.start_new_line(1);
        }
        let length = jsstr::len(&word) as isize;
        let line_start = self.words.is_empty();
        let cost = length + isize::from(!line_start);
        if cost <= self.available {
            self.words.push(word);
            self.available -= cost;
            return;
        }
        // Wrap before a word that does not fit; one longer than a line stays whole.
        if !line_start {
            self.start_new_line(1);
        }
        self.words.push(word);
        self.available -= length;
    }

    fn pop_word(&mut self) -> Option<String> {
        let word = self.words.pop()?;
        self.available += jsstr::len(&word) as isize + isize::from(!self.words.is_empty());
        Some(word)
    }

    fn concat_word(&mut self, word: String) {
        if self.word_break_opportunity && jsstr::len(&word) as isize > self.available {
            self.push_word(word);
            self.word_break_opportunity = false;
        } else {
            let mut joined = self.pop_word().unwrap_or_default();
            joined.push_str(&word);
            self.push_word(joined);
        }
    }

    fn start_new_line(&mut self, n: usize) {
        self.lines.push(std::mem::take(&mut self.words));
        for _ in 1..n {
            self.lines.push(Vec::new());
        }
        self.available = self.max_line_length as isize;
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.words.is_empty()
    }

    /// The lines joined into `out`, leaving the builder empty.
    fn drain_into(&mut self, out: &mut Rope) {
        self.available = self.max_line_length as isize;
        if self.is_empty() {
            return;
        }
        let mut text = String::new();
        for (index, words) in self.lines.iter().chain(std::iter::once(&self.words)).enumerate() {
            if index > 0 {
                text.push('\n');
            }
            for (position, word) in words.iter().enumerate() {
                if position > 0 {
                    text.push(' ');
                }
                text.push_str(word);
            }
        }
        out.push_str(&text);
        self.lines.clear();
        self.words.clear();
    }
}

/// Text built by appending, where joining two texts costs nothing: a block's
/// text is handed up through every enclosing block.
#[derive(Default)]
struct Rope {
    pieces: LinkedList<String>,
    length: usize,
}

impl Rope {
    fn from_string(text: String) -> Rope {
        Rope {
            length: text.len(),
            pieces: LinkedList::from([text]),
        }
    }

    fn is_empty(&self) -> bool {
        self.length == 0
    }

    fn push_str(&mut self, text: &str) {
        self.length += text.len();
        match self.pieces.back_mut() {
            Some(last) => last.push_str(text),
            None => self.pieces.push_back(text.to_string()),
        }
    }

    fn append(&mut self, mut other: Rope) {
        self.length += other.length;
        self.pieces.append(&mut other.pieces);
    }

    fn into_string(self) -> String {
        let mut pieces = self.pieces.into_iter();
        let mut text = pieces.next().unwrap_or_default();
        text.reserve(self.length - text.len());
        pieces.for_each(|piece| text.push_str(&piece));
        text
    }
}

struct Block {
    lines: LineBuilder,
    raw_text: Rope,
    leading_line_breaks: usize,
    stashed_line_breaks: usize,
    is_pre: bool,
}

impl Block {
    fn new(leading_line_breaks: usize, max_line_length: usize, is_pre: bool) -> Block {
        Block {
            lines: LineBuilder::new(max_line_length),
            raw_text: Rope::default(),
            leading_line_breaks,
            stashed_line_breaks: 0,
            is_pre,
        }
    }

    fn into_text(mut self) -> Rope {
        self.lines.drain_into(&mut self.raw_text);
        self.raw_text
    }

    /// Appends a finished child block's text with the line breaks between them.
    fn append(&mut self, text: Rope, leading_line_breaks: usize, trailing_line_breaks: usize) {
        self.lines.drain_into(&mut self.raw_text);
        let line_breaks = self.stashed_line_breaks.max(leading_line_breaks);
        if self.raw_text.is_empty() {
            self.raw_text = text;
            self.leading_line_breaks = line_breaks;
        } else {
            self.raw_text.push_str(&"\n".repeat(line_breaks));
            self.raw_text.append(text);
        }
        self.stashed_line_breaks = trailing_line_breaks;
    }
}

/// What a finished block's text goes through before its parent takes it.
enum BlockTransform {
    None,
    Quote,
    ListItem { prefix: String, max_prefix_length: usize },
}

impl BlockTransform {
    fn apply(&self, text: Rope) -> Rope {
        match self {
            BlockTransform::None => text,
            BlockTransform::Quote => {
                let text = text.into_string();
                let lines: Vec<String> = text
                    .trim_matches('\n')
                    .split('\n')
                    .map(|line| format!("> {line}"))
                    .collect();
                Rope::from_string(lines.join("\n"))
            }
            BlockTransform::ListItem {
                prefix,
                max_prefix_length,
            } => {
                let spacing = format!("\n{}", " ".repeat(*max_prefix_length));
                let padding = " ".repeat(max_prefix_length - jsstr::len(prefix));
                Rope::from_string(format!(
                    "{prefix}{padding}{}",
                    text.into_string().replace('\n', &spacing)
                ))
            }
        }
    }
}

struct TextBuilder {
    /// The open blocks, innermost last.
    blocks: Vec<Block>,
    /// How many enclosing elements uppercase their words.
    uppercase: usize,
    /// Lets a caller tell whether a subtree produced any words.
    words_added: usize,
    show_urls: bool,
}

impl TextBuilder {
    fn new(show_urls: bool) -> TextBuilder {
        TextBuilder {
            blocks: vec![Block::new(1, WORDWRAP, false)],
            uppercase: 0,
            words_added: 0,
            show_urls,
        }
    }

    fn block(&mut self) -> &mut Block {
        self.blocks.last_mut().expect("the root block stays open")
    }

    fn line_break(&mut self) {
        let block = self.block();
        if block.is_pre {
            block.raw_text.push_str("\n");
        } else {
            block.lines.start_new_line(1);
        }
    }

    fn word_break_opportunity(&mut self) {
        self.block().lines.word_break_opportunity = true;
    }

    fn add_inline(&mut self, text: &str, word_transform: bool) {
        let uppercase = word_transform && self.uppercase > 0;
        let mut words_added = 0;
        let block = self.block();
        if block.is_pre {
            block.raw_text.push_str(text);
            return;
        }
        let mut words = text.split(is_whitespace).filter(|word| !word.is_empty()).peekable();
        if text.is_empty() || (block.stashed_line_breaks > 0 && words.peek().is_none()) {
            return;
        }
        if block.stashed_line_breaks > 0 {
            block.lines.start_new_line(block.stashed_line_breaks);
        }

        // Whitespace runs collapse to single spaces while words are added.
        let lines = &mut block.lines;
        let stashed = lines.stashed_space;
        let any = words.peek().is_some();
        let mut joins = !stashed && !text.starts_with(is_whitespace);
        for word in words {
            words_added += usize::from(word_transform);
            let word = if uppercase {
                word.to_uppercase()
            } else {
                word.to_string()
            };
            if std::mem::take(&mut joins) {
                lines.concat_word(word);
            } else {
                lines.push_word(word);
            }
        }
        lines.stashed_space = (stashed && !any) || text.ends_with(is_whitespace);
        block.stashed_line_breaks = 0;
        self.words_added += words_added;
    }

    fn open_block(&mut self, leading_line_breaks: usize, reserved_line_length: usize, is_pre: bool) {
        let parent = self.block();
        let max_line_length = parent
            .lines
            .max_line_length
            .saturating_sub(reserved_line_length)
            .max(20);
        let is_pre = is_pre || parent.is_pre;
        self.blocks
            .push(Block::new(leading_line_breaks, max_line_length, is_pre));
    }

    fn close_block(&mut self, trailing_line_breaks: usize, transform: &BlockTransform) {
        let Some(block) = self.blocks.pop() else {
            return;
        };
        let leading = block.leading_line_breaks;
        let trailing = block.stashed_line_breaks.max(trailing_line_breaks);
        let text = transform.apply(block.into_text());
        self.block().append(text, leading, trailing);
    }

    fn finish(mut self) -> String {
        let root = self.blocks.swap_remove(0);
        root.into_text().into_string()
    }
}

/// The value of the digits of a hexadecimal, octal or binary literal, as
/// JavaScriptCore computes it: summed in floating point, and past 2^53
/// summed again from the last digit up, each step rounding.
fn radix_literal(digits: &str, radix: u32) -> Option<f64> {
    let digits: Vec<f64> = digits
        .chars()
        .map(|c| c.to_digit(radix).map(f64::from))
        .collect::<Option<_>>()?;
    if digits.is_empty() {
        return None;
    }
    let radix = f64::from(radix);
    let number = digits.iter().fold(0.0, |number, digit| number * radix + digit);
    if number < 9007199254740992.0 {
        return Some(number);
    }
    let mut number = 0.0;
    let mut multiplier = 1.0;
    for &digit in digits.iter().rev() {
        if multiplier == f64::INFINITY {
            if digit != 0.0 {
                return Some(f64::INFINITY);
            }
        } else {
            number += digit * multiplier;
        }
        multiplier *= radix;
    }
    Some(number)
}

/// `Number(value)` for a string.
fn string_to_number(value: &str) -> f64 {
    let value = jsstr::trim(value);
    if value.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        match value.get(..2) {
            Some(start) if start.eq_ignore_ascii_case(prefix) => {
                return radix_literal(&value[2..], radix).unwrap_or(f64::NAN);
            }
            _ => {}
        }
    }
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    if unsigned == "Infinity" {
        return if value.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // A decimal literal: digits with an optional fraction, or a fraction
    // alone, then an optional exponent.
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let all_digits = |digits: &str| digits.bytes().all(|b| b.is_ascii_digit());
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let exponent_ok = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        !digits.is_empty() && all_digits(digits)
    });
    if !exponent_ok || !all_digits(whole) || !all_digits(fraction) || (whole.is_empty() && fraction.is_empty()) {
        return f64::NAN;
    }
    value.parse().unwrap_or(f64::NAN)
}

/// A positive decimal as its digits and where the point falls: `0.digits × 10^point`.
#[derive(Clone, PartialEq, Debug)]
struct Decimal {
    digits: String,
    point: i32,
}

impl Decimal {
    fn new(digits: &str, point: i32) -> Decimal {
        let significant = digits.trim_start_matches('0');
        let point = point - (digits.len() - significant.len()) as i32;
        Decimal {
            digits: significant.trim_end_matches('0').to_string(),
            point,
        }
    }

    /// The exact value of a finite positive double.
    fn exact(value: f64) -> Decimal {
        // 1100 digits after the point hold any double exactly.
        let written = format!("{value:.1100}");
        let (whole, fraction) = written.split_once('.').unwrap_or((&written, ""));
        Decimal::new(&format!("{whole}{fraction}"), whole.len() as i32)
    }

    fn cmp(&self, other: &Decimal) -> std::cmp::Ordering {
        match (self.digits.is_empty(), other.digits.is_empty()) {
            (true, true) => return std::cmp::Ordering::Equal,
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        // With no leading zeros, the larger point is the larger number, and
        // equal points compare digit by digit.
        self.point
            .cmp(&other.point)
            .then_with(|| self.digits.cmp(&other.digits))
    }
}

/// The digits of `String(value)` for a positive finite number and where the
/// decimal point falls among them: the fewest digits that read back as
/// `value`, of those the closest to it, and of two equally close the even one.
fn shortest_digits(value: f64) -> (String, i32) {
    // The fewest digits, though on a tie not always the even ones.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits = mantissa.replace('.', "");
    let point = exponent.parse::<i32>().unwrap_or(0) + 1;

    // Its neighbours with as many digits may read back as the value too.
    let reads_back = |candidate: &str| format!("0.{candidate}e{point}").parse::<f64>() == Ok(value);
    let neighbour = |step: i8| -> Option<String> {
        let mut neighbour: Vec<u8> = digits.bytes().collect();
        for digit in neighbour.iter_mut().rev() {
            match (step, *digit) {
                (1, b'9') => *digit = b'0',
                (-1, b'0') => *digit = b'9',
                _ => {
                    *digit = (*digit as i8 + step) as u8;
                    let neighbour = String::from_utf8(neighbour).ok()?;
                    // One that lost or gained a digit is not a neighbour.
                    return (!neighbour.starts_with('0')).then_some(neighbour);
                }
            }
        }
        None
    };
    let mut best = digits.clone();
    let exact = Decimal::exact(value);
    for other in [neighbour(-1), neighbour(1)]
        .into_iter()
        .flatten()
        .filter(|other| reads_back(other))
    {
        let (low, high) = if other < best { (&other, &best) } else { (&best, &other) };
        // Halfway between the two: the lower one with a 5 after it.
        let halfway = Decimal::new(&format!("{low}5"), point);
        let low_is_even = low.as_bytes()[low.len() - 1] % 2 == 0;
        best = match exact.cmp(&halfway) {
            std::cmp::Ordering::Less => low.clone(),
            std::cmp::Ordering::Greater => high.clone(),
            std::cmp::Ordering::Equal if low_is_even => low.clone(),
            std::cmp::Ordering::Equal => high.clone(),
        };
    }
    (best, point)
}

/// `String(value)` for a number.
fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let (digits, point) = shortest_digits(value.abs());
    let count = digits.len() as i32;
    if count <= point && point <= 21 {
        format!("{sign}{digits}{}", "0".repeat((point - count) as usize))
    } else if 0 < point && point <= 21 {
        format!("{sign}{}.{}", &digits[..point as usize], &digits[point as usize..])
    } else if -6 < point && point <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat(-point as usize))
    } else {
        let exponent = point - 1;
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        let fraction = if count > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        format!("{sign}{}{fraction}e{exponent_sign}{}", &digits[..1], exponent.abs())
    }
}

/// `a`, `b`, … `z`, `aa`, … counted from `base`, with the arithmetic done in
/// floating point as there. An infinite number never ends there; here it is
/// written as the number.
fn letter_sequence(mut n: f64, base: char) -> String {
    if n == f64::INFINITY {
        return number_to_string(n);
    }
    let mut digits = Vec::new();
    loop {
        n -= 1.0;
        digits.push(n % 26.0);
        n = (n / 26.0).floor();
        // `n > 0` is false for NaN.
        if n.is_nan() || n <= 0.0 {
            break;
        }
    }
    // `String.fromCharCode` takes the sum modulo 2^16, NaN as 0.
    digits
        .iter()
        .rev()
        .map(|digit| {
            let code = base as u32 as f64 + digit;
            let unit = if code.is_nan() {
                0
            } else {
                (code.trunc() as i64).rem_euclid(65536) as u32
            };
            char::from_u32(unit).unwrap_or('\u{fffd}')
        })
        .collect()
}

const ROMAN_ONES: [&str; 4] = ["I", "X", "C", "M"];
const ROMAN_FIVES: [&str; 3] = ["V", "L", "D"];

/// The Roman numeral for `n`, built digit by digit from `String(n)` as
/// there: past the thousands the symbol tables run out, which reads as
/// `undefined`, `NaN` or a thrown `TypeError` (`None` here).
fn roman(n: f64) -> Option<String> {
    let text = number_to_string(n);
    let digits: Vec<f64> = text
        .chars()
        .map(|c| c.to_digit(10).map_or(f64::NAN, f64::from))
        .collect();
    let mut numerals = Vec::with_capacity(digits.len());
    for (i, &v) in digits.iter().rev().enumerate() {
        let ones = ROMAN_ONES.get(i).copied();
        let numeral = if v % 5.0 < 4.0 {
            let five = if v < 5.0 {
                ""
            } else {
                ROMAN_FIVES.get(i).copied().unwrap_or("undefined")
            };
            format!("{five}{}", ones?.repeat((v % 5.0) as usize))
        } else {
            let next = if v < 5.0 {
                ROMAN_FIVES.get(i).copied()
            } else {
                ROMAN_ONES.get(i + 1).copied()
            };
            match (ones, next) {
                // `undefined + undefined`
                (None, None) => "NaN".to_string(),
                (ones, next) => format!("{}{}", ones.unwrap_or("undefined"), next.unwrap_or("undefined")),
            }
        };
        numerals.push(numeral);
    }
    numerals.reverse();
    Some(numerals.concat())
}

#[derive(Clone, Copy)]
enum Marker {
    Bullet,
    Number,
    LowerLetter,
    UpperLetter,
    LowerRoman,
    UpperRoman,
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "article" | "aside" | "div" | "footer" | "form" | "header" | "main" | "nav" | "section"
    )
}

/// What is left to do, innermost last.
enum Step {
    /// The children of an element from `next` on.
    Children {
        element: NodeId,
        next: usize,
    },
    Node(NodeId),
    CloseBlock {
        trailing_line_breaks: usize,
        transform: BlockTransform,
    },
    EndUppercase,
    /// After a link's text: its target, in brackets when there was text.
    AnchorTarget {
        href: String,
        words_before: usize,
    },
    /// The items of a list from `next` on, each in a block of its own.
    ListItems {
        items: Vec<(NodeId, String)>,
        next: usize,
        max_prefix_length: usize,
        nested: bool,
    },
}

struct Renderer<'a> {
    nodes: &'a [Node],
    builder: TextBuilder,
    steps: Vec<Step>,
    failure: Option<PlainTextFailure>,
}

impl<'a> Renderer<'a> {
    fn element(&self, id: NodeId) -> &'a Element {
        let nodes = self.nodes;
        match &nodes[id] {
            Node::Element(element) => element,
            _ => unreachable!("only elements are parents"),
        }
    }

    fn run(&mut self) {
        let nodes = self.nodes;
        while let Some(step) = self.steps.pop() {
            match step {
                Step::Children { element, next } => {
                    if let Some(&child) = self.element(element).children.get(next) {
                        self.steps.push(Step::Children {
                            element,
                            next: next + 1,
                        });
                        self.steps.push(Step::Node(child));
                    }
                }
                Step::Node(id) => match &nodes[id] {
                    Node::Text(text) => self.builder.add_inline(&decode_entities(text), true),
                    Node::Element(element) if !is_skipped(&element.name) => self.format(id),
                    _ => {}
                },
                Step::CloseBlock {
                    trailing_line_breaks,
                    transform,
                } => {
                    self.builder.close_block(trailing_line_breaks, &transform);
                }
                Step::EndUppercase => self.builder.uppercase -= 1,
                Step::AnchorTarget { href, words_before } => {
                    let has_text = self.builder.words_added > words_before;
                    let target = if has_text { format!(" [{href}]") } else { href };
                    self.builder.add_inline(&target, false);
                }
                Step::ListItems {
                    items,
                    next,
                    max_prefix_length,
                    nested,
                } => {
                    let Some((node, prefix)) = items.get(next).cloned() else {
                        self.builder
                            .close_block(if nested { 1 } else { 2 }, &BlockTransform::None);
                        continue;
                    };
                    self.steps.push(Step::ListItems {
                        items,
                        next: next + 1,
                        max_prefix_length,
                        nested,
                    });
                    self.builder.open_block(1, max_prefix_length, false);
                    self.steps.push(Step::CloseBlock {
                        trailing_line_breaks: 1,
                        transform: BlockTransform::ListItem {
                            prefix,
                            max_prefix_length,
                        },
                    });
                    self.steps.push(Step::Node(node));
                }
            }
        }
    }

    /// Opens a block around an element's children.
    fn block(&mut self, id: NodeId, leading: usize, trailing: usize, reserved: usize, transform: BlockTransform) {
        let is_pre = self.element(id).name == "pre";
        self.builder.open_block(leading, reserved, is_pre);
        self.steps.push(Step::CloseBlock {
            trailing_line_breaks: trailing,
            transform,
        });
        self.steps.push(Step::Children { element: id, next: 0 });
    }

    fn format(&mut self, id: NodeId) {
        let element = self.element(id);
        let name = element.name.as_str();
        if is_block(name) {
            return self.block(id, 1, 1, 0, BlockTransform::None);
        }
        match name {
            "img" => {
                if self.builder.show_urls {
                    let alt = decode_attribute_entities(element.attrib("alt").unwrap_or(""));
                    let src = decode_attribute_entities(element.attrib("src").unwrap_or(""));
                    let text = match (src.is_empty(), alt.is_empty()) {
                        (true, _) => alt,
                        (false, true) => format!("[{src}]"),
                        (false, false) => format!("{alt} [{src}]"),
                    };
                    self.builder.add_inline(&text, false);
                }
            }
            "a" => {
                // Link text, then the target in brackets; a link with no
                // text shows the bare target.
                if self.builder.show_urls {
                    let href = decode_attribute_entities(element.attrib("href").unwrap_or(""));
                    let href = href.strip_prefix("mailto:").unwrap_or(&href);
                    if !href.is_empty() && !href.starts_with('#') {
                        let words_before = self.builder.words_added;
                        self.steps.push(Step::AnchorTarget {
                            href: href.to_string(),
                            words_before,
                        });
                    }
                }
                self.steps.push(Step::Children { element: id, next: 0 });
            }
            "br" => self.builder.line_break(),
            "wbr" => self.builder.word_break_opportunity(),
            "p" | "table" | "pre" => self.block(id, 2, 2, 0, BlockTransform::None),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.builder.open_block(if name <= "h3" { 3 } else { 2 }, 0, false);
                self.builder.uppercase += 1;
                self.steps.push(Step::CloseBlock {
                    trailing_line_breaks: 2,
                    transform: BlockTransform::None,
                });
                self.steps.push(Step::EndUppercase);
                self.steps.push(Step::Children { element: id, next: 0 });
            }
            "hr" => {
                self.builder.open_block(2, 0, false);
                self.builder.add_inline(&"-".repeat(WORDWRAP), true);
                self.builder.close_block(2, &BlockTransform::None);
            }
            "blockquote" => self.block(id, 2, 2, 2, BlockTransform::Quote),
            "ul" => self.format_list(id, Marker::Bullet),
            "ol" => {
                let marker = match element.attrib("type") {
                    Some("a") => Marker::LowerLetter,
                    Some("A") => Marker::UpperLetter,
                    Some("i") => Marker::LowerRoman,
                    Some("I") => Marker::UpperRoman,
                    _ => Marker::Number,
                };
                self.format_list(id, marker);
            }
            _ => self.steps.push(Step::Children { element: id, next: 0 }),
        }
    }

    fn format_list(&mut self, id: NodeId, marker: Marker) {
        let nodes = self.nodes;
        let element = self.element(id);
        let nested = element.parent.is_some_and(|parent| self.element(parent).name == "li");
        let mut index = string_to_number(element.attrib("start").filter(|start| !start.is_empty()).unwrap_or("1"));

        let mut max_prefix_length = 0;
        let mut items: Vec<(NodeId, String)> = Vec::new();
        for &child in &element.children {
            let prefix = match &nodes[child] {
                // Text between the items counts only when it has something to show.
                Node::Text(text) if jsstr::trim(text).is_empty() => continue,
                Node::Element(item) if item.name == "li" => {
                    let n = index;
                    index += 1.0;
                    let numeral = |n: f64| roman(n).ok_or(PlainTextFailure::RomanNumeral);
                    let marker = match marker {
                        Marker::Bullet => Ok("*".to_string()),
                        Marker::Number => Ok(format!("{}.", number_to_string(n))),
                        Marker::LowerLetter => Ok(format!("{}.", letter_sequence(n, 'a'))),
                        Marker::UpperLetter => Ok(format!("{}.", letter_sequence(n, 'A'))),
                        Marker::LowerRoman => numeral(n).map(|numeral| format!("{}.", numeral.to_lowercase())),
                        Marker::UpperRoman => numeral(n).map(|numeral| format!("{numeral}.")),
                    };
                    let marker = marker.unwrap_or_else(|failure| {
                        self.failure.get_or_insert(failure);
                        format!("{}.", number_to_string(n))
                    });
                    if nested {
                        format!("{marker} ")
                    } else {
                        format!(" {marker} ")
                    }
                }
                _ => String::new(),
            };
            max_prefix_length = max_prefix_length.max(jsstr::len(&prefix));
            items.push((child, prefix));
        }
        if items.is_empty() {
            return;
        }
        self.builder.open_block(if nested { 1 } else { 2 }, 0, false);
        self.steps.push(Step::ListItems {
            items,
            next: 0,
            max_prefix_length,
            nested,
        });
    }
}

/// `<body>` elements when there are any, else the whole document.
fn find_bodies(nodes: &[Node]) -> Vec<NodeId> {
    let mut found = Vec::new();
    let mut pending = vec![(ROOT, 0)];
    while let Some((id, next)) = pending.pop() {
        let Node::Element(element) = &nodes[id] else {
            continue;
        };
        let Some(&child) = element.children.get(next) else {
            continue;
        };
        pending.push((id, next + 1));
        match &nodes[child] {
            Node::Element(element) if element.name == "body" => found.push(child),
            Node::Element(element) if !is_skipped(&element.name) => pending.push((child, 0)),
            _ => {}
        }
    }
    found
}

/// Longer input is cut off; the text of a 16 MiB mail is not worth more.
const MAX_INPUT_LENGTH: usize = 1 << 24;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlainTextOptions {
    /// Follow each link's text and each image's alt text with its URL in brackets.
    pub urls: bool,
}

/// The plain-text rendering of `html`.
///
/// The TypeScript renderer throws on a few inputs; those are an `Err` that
/// still carries the text. It also throws once elements nest past what its
/// call stack holds (some thousands deep), which has no counterpart here:
/// any depth renders.
pub fn html_to_plain_text(html: &str, options: PlainTextOptions) -> Result<String, PlainTextError> {
    let truncated;
    let html = if html.len() > MAX_INPUT_LENGTH && jsstr::len(html) > MAX_INPUT_LENGTH {
        truncated = jsstr::truncate(html, MAX_INPUT_LENGTH);
        truncated.as_str()
    } else {
        html
    };
    let mut tree = TreeBuilder::new();
    tokenize(html, &mut tree);

    let bodies = find_bodies(&tree.nodes);
    let mut renderer = Renderer {
        nodes: &tree.nodes,
        builder: TextBuilder::new(options.urls),
        steps: Vec::new(),
        failure: tree.failure,
    };
    if bodies.is_empty() {
        renderer.steps.push(Step::Children { element: ROOT, next: 0 });
    } else {
        renderer.steps.extend(bodies.into_iter().rev().map(Step::Node));
    }
    renderer.run();
    let rendered = renderer.builder.finish();
    match renderer.failure {
        Some(reason) => Err(PlainTextError { reason, rendered }),
        None => Ok(rendered),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(html: &str) -> String {
        html_to_plain_text(html, PlainTextOptions::default()).unwrap()
    }

    fn text_with_urls(html: &str) -> String {
        html_to_plain_text(html, PlainTextOptions { urls: true }).unwrap()
    }

    #[test]
    fn renders_markup_as_text() {
        let cases: [(&str, &str, &str); 19] = [
            (
                "separates paragraphs and keeps line breaks",
                "<p>First</p><p>Second<br>line</p>text after",
                "First\n\nSecond\nline\n\ntext after",
            ),
            (
                "uppercases headings",
                "<h1>Title</h1><h4>Small heading</h4><p>body</p>",
                "TITLE\n\nSMALL HEADING\n\nbody",
            ),
            (
                "prefixes list items, nested lists indented",
                "<ul><li>one</li><li>two<ul><li>nested</li></ul></li></ul>",
                " * one\n * two\n   * nested",
            ),
            (
                "numbers ordered lists from start, in the given style",
                "<ol start=\"3\"><li>c</li><li>d</li></ol><ol type=\"I\"><li>x</li><li>y</li><li>z</li>\
                 <li>w</li></ol><ol type=\"a\"><li>p</li></ol>",
                " 3. c\n 4. d\n\n I.   x\n II.  y\n III. z\n IV.  w\n\n a. p",
            ),
            (
                "prefixes quoted lines",
                "<p>On Monday someone wrote:</p><blockquote><p>quoted</p><p>more</p></blockquote>",
                "On Monday someone wrote:\n\n> quoted\n> \n> more",
            ),
            (
                "wraps at 80 columns",
                "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor \
                 incididunt ut labore et dolore magna aliqua.</p>",
                "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor\n\
                 incididunt ut labore et dolore magna aliqua.",
            ),
            (
                "puts a word longer than a line on its own line",
                "<p>short https://example.com/a/very/long/path/that/keeps/going/and/going/past/eighty/\
                 columns end</p>",
                "short\nhttps://example.com/a/very/long/path/that/keeps/going/and/going/past/eighty/\
                 columns\nend",
            ),
            (
                "keeps whitespace in pre",
                "<pre>  a\n    b  c</pre><p>after</p>",
                "  a\n    b  c\n\nafter",
            ),
            (
                "runs table cells together",
                "<table><tr><td>Name</td><td>Qty</td></tr><tr><td>Apple</td><td>3</td></tr></table>",
                "NameQtyApple3",
            ),
            (
                "closes open items and paragraphs",
                "<ul><li>one<li>two</ul><p>a<p>b",
                " * one\n * two\n\na\n\nb",
            ),
            (
                "drops scripts, styles, comments and images",
                "<style>p{color:red}</style><p>a<!-- hidden -->b<img src=x alt=pic></p>\
                 <script>var s = '<p>no</p>';</script>",
                "ab",
            ),
            (
                "reads only the body when there is one",
                "<html><head><title>Subject</title></head><body><p>Body</p></body></html>",
                "Body",
            ),
            (
                "reads everything without a body",
                "<title>Subject</title><p>Body</p>",
                "Subject\n\nBody",
            ),
            (
                "keeps only link text",
                "<p>Visit <a href=\"https://example.com\">our site</a>.</p>",
                "Visit our site.",
            ),
            (
                "draws a horizontal rule",
                "<p>a</p><hr><p>b</p>",
                &format!("a\n\n{}\n\nb", "-".repeat(80)),
            ),
            (
                "collapses whitespace",
                "<p>  Too   many \n\t spaces  </p>",
                "Too many spaces",
            ),
            (
                "ends script content only at its own end tag",
                "<script>if (a</b) {}</scr ipt></script>after",
                "after",
            ),
            (
                "turns stray end tags into elements or nothing",
                "</p>x</br>y</div>z",
                "\n\nx\nyz",
            ),
            (
                "decodes character references",
                "<p>&lt;a&gt; &amp; &quot;q&quot; &#169; &#x263A; caf&eacute; AT&T &copy=1</p>",
                "<a> & \"q\" © ☺ café AT&T ©=1",
            ),
        ];
        for (name, html, expected) in cases {
            assert_eq!(text(html), expected, "{name}");
        }
    }

    #[test]
    fn drops_link_targets_and_images_unless_asked_for_urls() {
        let html = "<p>Verify: <a href=\"https://bit.ly/3abc\">now</a><img src=\"a.png\" alt=\"A\"></p>";
        assert_eq!(text(html), "Verify: now");
        assert_eq!(text_with_urls(html), "Verify: now [https://bit.ly/3abc]A [a.png]");
    }

    #[test]
    fn shows_link_targets_and_image_sources_with_urls() {
        let cases = [
            (
                "<a href=\"https://x.org\">https://x.org</a>",
                "https://x.org [https://x.org]",
            ),
            (
                "<a href=\"mailto:a@b.c\">write</a> <a href=\"mailto:a@b.c\"></a>",
                "write [a@b.c] a@b.c",
            ),
            (
                "<a href=\"#top\">top</a> <a href=\"\">empty</a> <a>none</a>",
                "top empty none",
            ),
            (
                "<a href=\"https://x.org/?a=1&amp;b=2&copy=3&#38;c\">q</a>",
                "q [https://x.org/?a=1&b=2&copy=3&c]",
            ),
            (
                "<img src=\"cid:logo@x\" alt=\"Logo\"> <img src=\"y.png\"> <img alt=\"alt\">",
                "Logo [cid:logo@x] [y.png] alt",
            ),
            (
                "<h1><a href=\"https://x.org/h\">link</a></h1>",
                "LINK [https://x.org/h]",
            ),
        ];
        for (html, expected) in cases {
            assert_eq!(text_with_urls(html), expected);
        }
    }

    #[test]
    fn handles_unterminated_markup_at_the_end_of_input() {
        assert_eq!(text("text <"), "text <");
        assert_eq!(text("<p>a<!-- open"), "a");
        assert_eq!(text("<p>a</p><div class=\"x"), "a");
    }

    #[test]
    fn stays_linear_on_long_runs_of_line_breaks_newlines_and_references() {
        // Quadratic handling of any of these takes seconds to minutes at this size.
        let inputs = [
            format!("<blockquote><p>a</p>{}<p>b</p></blockquote>", "<br>".repeat(100_000)),
            format!("<blockquote><pre>a{}b</pre></blockquote>", "\n".repeat(400_000)),
            format!(
                "<blockquote>{}{}{}</blockquote>",
                "&#".repeat(200_000),
                "&amp;".repeat(100_000),
                "&notit;".repeat(50_000)
            ),
        ];
        for html in inputs {
            let started = std::time::Instant::now();
            let rendered = text(&html);
            assert!(started.elapsed().as_millis() < 3000);
            assert!(rendered.starts_with("> "));
        }
    }

    #[test]
    fn stays_linear_on_deeply_nested_links_with_urls() {
        let html = "<a href=\"x\">w ".repeat(6000);
        let started = std::time::Instant::now();
        let rendered = text_with_urls(&html);
        assert!(started.elapsed().as_millis() < 1000);
        assert!(rendered.starts_with("w w"));
        assert!(rendered.ends_with("[x]"));
    }

    #[test]
    fn converts_large_input() {
        let row = "<tr><td>cell <b>bold</b> &amp; text</td></tr>";
        let rendered = text(&format!("<table>{}</table>", row.repeat(50000)));
        assert!(rendered.starts_with("cell bold & textcell bold"));
        assert!(rendered.len() > 50000 * 15);
    }

    #[test]
    fn renders_nesting_deeper_than_a_call_stack() {
        let rendered = text(&format!("{}x", "<div><b>".repeat(200_000)));
        assert_eq!(rendered, "x");
        let quoted = text(&format!("{}x", "<blockquote>".repeat(2000)));
        assert_eq!(quoted, format!("{}x", "> ".repeat(2000)));
    }

    #[test]
    fn reports_what_the_typescript_renderer_throws_on() {
        let failed = html_to_plain_text("<p><constructor>x", PlainTextOptions::default()).unwrap_err();
        assert_eq!(failed.reason, PlainTextFailure::ConstructorTag);
        assert_eq!(failed.rendered, "x");
        assert_eq!(text("<constructor>x"), "x");

        let failed = html_to_plain_text(
            "<ol type=\"I\" start=\"10000\"><li>a</li></ol>",
            PlainTextOptions::default(),
        )
        .unwrap_err();
        assert_eq!(failed.reason, PlainTextFailure::RomanNumeral);
        assert_eq!(failed.rendered, " 10000. a");
    }

    #[test]
    fn numbers_convert_as_in_javascript() {
        assert_eq!(string_to_number(" 12 "), 12.0);
        assert_eq!(string_to_number("0x1F"), 31.0);
        assert_eq!(string_to_number("0o7705173430276017417"), 142047800249032460.0);
        assert_eq!(string_to_number("0x20000000000001"), 9007199254740992.0);
        assert_eq!(string_to_number("0x20000000000003"), 9007199254740996.0);
        assert_eq!(string_to_number("0x1fffffffffffff4"), 144115188075855860.0);
        assert_eq!(string_to_number("0x200000000000010000001"), 2.417851639229259e24);
        // Summed in floating point from the last digit up, not rounded once.
        assert_eq!(string_to_number("0x67e83FAf5da7CDEb93Ed"), 4.906879813501947e23);
        assert_eq!(
            string_to_number(&format!("0b1{}11", "0".repeat(52))),
            18014398509481988.0
        );
        assert_eq!(
            string_to_number(&format!("0x{}", "f".repeat(255))),
            1.1235582092889474e307
        );
        assert_eq!(string_to_number(&format!("0x{}", "f".repeat(256))), f64::INFINITY);
        assert!(string_to_number("0x").is_nan());
        assert!(string_to_number("0b12").is_nan());
        assert!(string_to_number("1_0").is_nan());
        assert!(string_to_number("inf").is_nan());
        assert_eq!(string_to_number("1e400"), f64::INFINITY);
        assert_eq!(number_to_string(1e21), "1e+21");
        assert_eq!(number_to_string(1e-7), "1e-7");
        assert_eq!(number_to_string(1.0000001), "1.0000001");
        assert_eq!(number_to_string(0.000001), "0.000001");
        assert_eq!(
            number_to_string(123456789012345680000000000000.0),
            "1.2345678901234568e+29"
        );
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(-2.5), "-2.5");
        // Two shortest forms equally close: the even one.
        assert_eq!(number_to_string(893514022863146.25), "893514022863146.2");
        assert_eq!(number_to_string(5e-324), "5e-324");
        assert_eq!(number_to_string(1.7976931348623157e308), "1.7976931348623157e+308");
    }

    #[test]
    fn list_markers_follow_the_javascript_arithmetic() {
        assert_eq!(
            text("<ol type=\"I\" start=\"3999\"><li>a</li><li>b</li></ol>"),
            " MMMCMXCIX.  a\n Mundefined. b"
        );
        assert_eq!(text("<ol type=\"I\" start=\"40000\"><li>a</li></ol>"), " NaN. a");
        assert_eq!(
            text("<ol type=\"i\" start=\"49999\"><li>a</li></ol>"),
            " nanmundefinedcmxcix. a"
        );
        assert_eq!(text("<ol type=\"I\" start=\"-5\"><li>a</li></ol>"), " XCV. a");
        assert_eq!(
            text("<ol type=\"a\" start=\"0\"><li>a</li><li>b</li></ol>"),
            " `. a\n a. b"
        );
        assert_eq!(
            text("<ol type=\"A\" start=\"27\"><li>a</li><li>b</li></ol>"),
            " AA. a\n AB. b"
        );
        assert_eq!(text("<ol type=\"a\" start=\"abc\"><li>a</li></ol>"), " \u{0}. a");
    }
}
