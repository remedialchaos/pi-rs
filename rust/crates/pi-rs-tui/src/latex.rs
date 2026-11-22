// Ported from packages/tui/src/latex.ts
//
// Renders a LaTeX math expression as terminal-friendly Unicode text. The glyph widths and the
// layout algorithm come from the upstream tables: the output is byte for byte the upstream's.

use crate::utils::visible_width;

const SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ϵ"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("varkappa", "ϰ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("pi", "π"),
    ("varpi", "ϖ"),
    ("rho", "ρ"),
    ("varrho", "ϱ"),
    ("sigma", "σ"),
    ("varsigma", "ς"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "ϕ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Upsilon", "Υ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
    ("pm", "±"),
    ("mp", "∓"),
    ("times", "×"),
    ("div", "÷"),
    ("cdot", "·"),
    ("ast", "∗"),
    ("star", "⋆"),
    ("circ", "∘"),
    ("bullet", "•"),
    ("oplus", "⊕"),
    ("ominus", "⊖"),
    ("otimes", "⊗"),
    ("oslash", "⊘"),
    ("odot", "⊙"),
    ("bigcirc", "○"),
    ("dagger", "†"),
    ("ddagger", "‡"),
    ("amalg", "⨿"),
    ("uplus", "⊎"),
    ("sqcap", "⊓"),
    ("sqcup", "⊔"),
    ("bowtie", "⋈"),
    ("Join", "⋈"),
    ("ltimes", "⋉"),
    ("rtimes", "⋊"),
    ("leftouterjoin", "⟕"),
    ("rightouterjoin", "⟖"),
    ("fullouterjoin", "⟗"),
    ("triangleleft", "◁"),
    ("triangleright", "▷"),
    ("wr", "≀"),
    ("cap", "∩"),
    ("cup", "∪"),
    ("bigcap", "⋂"),
    ("bigcup", "⋃"),
    ("bigwedge", "⋀"),
    ("bigvee", "⋁"),
    ("bigsqcup", "⨆"),
    ("biguplus", "⨄"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigodot", "⨀"),
    ("setminus", "∖"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("subset", "⊂"),
    ("supset", "⊃"),
    ("subseteq", "⊆"),
    ("supseteq", "⊇"),
    ("sqsubset", "⊏"),
    ("sqsupset", "⊐"),
    ("sqsubseteq", "⊑"),
    ("sqsupseteq", "⊒"),
    ("prec", "≺"),
    ("preceq", "≼"),
    ("succ", "≻"),
    ("succeq", "≽"),
    ("ll", "≪"),
    ("gg", "≫"),
    ("le", "≤"),
    ("leq", "≤"),
    ("leqslant", "≤"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("geqslant", "≥"),
    ("ne", "≠"),
    ("neq", "≠"),
    ("equiv", "≡"),
    ("approx", "≈"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("cong", "≅"),
    ("asymp", "≍"),
    ("doteq", "≐"),
    ("propto", "∝"),
    ("parallel", "∥"),
    ("perp", "⊥"),
    ("mid", "∣"),
    ("vdash", "⊢"),
    ("dashv", "⊣"),
    ("models", "⊨"),
    ("Vdash", "⊩"),
    ("Vvdash", "⊪"),
    ("nvdash", "⊬"),
    ("nvDash", "⊭"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("neg", "¬"),
    ("land", "∧"),
    ("wedge", "∧"),
    ("lor", "∨"),
    ("vee", "∨"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("longrightarrow", "→"),
    ("leftarrow", "←"),
    ("longleftarrow", "←"),
    ("gets", "←"),
    ("leftrightarrow", "↔"),
    ("longleftrightarrow", "↔"),
    ("hookleftarrow", "↩"),
    ("hookrightarrow", "↪"),
    ("twoheadleftarrow", "↞"),
    ("twoheadrightarrow", "↠"),
    ("leftharpoonup", "↼"),
    ("leftharpoondown", "↽"),
    ("rightharpoonup", "⇀"),
    ("rightharpoondown", "⇁"),
    ("rightleftharpoons", "⇌"),
    ("leftrightharpoons", "⇋"),
    ("nearrow", "↗"),
    ("searrow", "↘"),
    ("swarrow", "↙"),
    ("nwarrow", "↖"),
    ("rightsquigarrow", "⇝"),
    ("leadsto", "⇝"),
    ("Rightarrow", "⇒"),
    ("Longrightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("Longleftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("Longleftrightarrow", "⇔"),
    ("implies", "⇒"),
    ("iff", "⇔"),
    ("mapsto", "↦"),
    ("longmapsto", "↦"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("int", "∫"),
    ("iint", "∬"),
    ("iiint", "∭"),
    ("oint", "∮"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("coprod", "∐"),
    ("infty", "∞"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("angle", "∠"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("aleph", "ℵ"),
    ("beth", "ℶ"),
    ("gimel", "ℷ"),
    ("daleth", "ℸ"),
    ("top", "⊤"),
    ("bot", "⊥"),
    ("triangle", "△"),
    ("square", "□"),
    ("lozenge", "◊"),
    ("checkmark", "✓"),
    ("complement", "∁"),
    ("wp", "℘"),
    ("prime", "′"),
    ("ldots", "…"),
    ("dots", "…"),
    ("cdots", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("ell", "ℓ"),
    ("hbar", "ℏ"),
    ("Im", "ℑ"),
    ("Re", "ℜ"),
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("vert", "|"),
    ("lvert", "|"),
    ("rvert", "|"),
    ("Vert", "‖"),
    ("lVert", "‖"),
    ("rVert", "‖"),
    ("lbrace", "{"),
    ("rbrace", "}"),
    ("backslash", "\\"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("colon", ":"),
];

const NEGATED_SYMBOLS: &[(&str, &str)] = &[
    ("<", "≮"),
    (">", "≯"),
    ("=", "≠"),
    ("∈", "∉"),
    ("∋", "∌"),
    ("∣", "∤"),
    ("∥", "∦"),
    ("∼", "≁"),
    ("≃", "≄"),
    ("≅", "≇"),
    ("≈", "≉"),
    ("≡", "≢"),
    ("≤", "≰"),
    ("≥", "≱"),
    ("≺", "⊀"),
    ("≻", "⊁"),
    ("⊂", "⊄"),
    ("⊃", "⊅"),
    ("⊆", "⊈"),
    ("⊇", "⊉"),
    ("⊢", "⊬"),
    ("⊨", "⊭"),
    ("↔", "↮"),
    ("←", "↚"),
    ("→", "↛"),
    ("⇒", "⇏"),
    ("⇐", "⇍"),
    ("⇔", "⇎"),
    ("≼", "⋠"),
    ("≽", "⋡"),
];

const BLACKBOARD: &[(&str, &str)] = &[
    ("C", "ℂ"),
    ("H", "ℍ"),
    ("N", "ℕ"),
    ("P", "ℙ"),
    ("Q", "ℚ"),
    ("R", "ℝ"),
    ("Z", "ℤ"),
];

const SUPERSCRIPTS: &[(&str, &str)] = &[
    ("0", "⁰"),
    ("1", "¹"),
    ("2", "²"),
    ("3", "³"),
    ("4", "⁴"),
    ("5", "⁵"),
    ("6", "⁶"),
    ("7", "⁷"),
    ("8", "⁸"),
    ("9", "⁹"),
    ("+", "⁺"),
    ("-", "⁻"),
    ("=", "⁼"),
    ("(", "⁽"),
    (")", "⁾"),
    ("a", "ᵃ"),
    ("b", "ᵇ"),
    ("c", "ᶜ"),
    ("d", "ᵈ"),
    ("e", "ᵉ"),
    ("f", "ᶠ"),
    ("g", "ᵍ"),
    ("h", "ʰ"),
    ("i", "ⁱ"),
    ("j", "ʲ"),
    ("k", "ᵏ"),
    ("l", "ˡ"),
    ("m", "ᵐ"),
    ("n", "ⁿ"),
    ("o", "ᵒ"),
    ("p", "ᵖ"),
    ("r", "ʳ"),
    ("s", "ˢ"),
    ("t", "ᵗ"),
    ("u", "ᵘ"),
    ("v", "ᵛ"),
    ("w", "ʷ"),
    ("x", "ˣ"),
    ("y", "ʸ"),
    ("z", "ᶻ"),
];

const SUBSCRIPTS: &[(&str, &str)] = &[
    ("0", "₀"),
    ("1", "₁"),
    ("2", "₂"),
    ("3", "₃"),
    ("4", "₄"),
    ("5", "₅"),
    ("6", "₆"),
    ("7", "₇"),
    ("8", "₈"),
    ("9", "₉"),
    ("+", "₊"),
    ("-", "₋"),
    ("=", "₌"),
    ("(", "₍"),
    (")", "₎"),
    ("a", "ₐ"),
    ("e", "ₑ"),
    ("h", "ₕ"),
    ("i", "ᵢ"),
    ("j", "ⱼ"),
    ("k", "ₖ"),
    ("l", "ₗ"),
    ("m", "ₘ"),
    ("n", "ₙ"),
    ("o", "ₒ"),
    ("p", "ₚ"),
    ("r", "ᵣ"),
    ("s", "ₛ"),
    ("t", "ₜ"),
    ("u", "ᵤ"),
    ("v", "ᵥ"),
    ("x", "ₓ"),
];

const ACCENTS: &[(&str, &str)] = &[
    ("acute", "\u{301}"),
    ("bar", "\u{305}"),
    ("breve", "\u{306}"),
    ("check", "\u{30c}"),
    ("ddot", "\u{308}"),
    ("dot", "\u{307}"),
    ("grave", "\u{300}"),
    ("hat", "\u{302}"),
    ("mathring", "\u{30a}"),
    ("overleftarrow", "⃖"),
    ("overleftrightarrow", "⃡"),
    ("overline", "\u{305}"),
    ("overrightarrow", "⃗"),
    ("tilde", "\u{303}"),
    ("underline", "\u{332}"),
    ("vec", "⃗"),
    ("widehat", "\u{302}"),
    ("widetilde", "\u{303}"),
];

const NAMED_OPERATORS: &[&str] = &[
    "arccos", "arcsin", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det", "dim",
    "exp", "gcd", "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log", "max", "min",
    "Pr", "sec", "sin", "sinh", "sup", "tan", "tanh",
];

const LIMIT_OPERATORS: &[&str] = &[
    "argmax", "argmin", "inf", "injlim", "lim", "liminf", "limsup", "max", "min", "projlim", "sup",
];

const DISPLAY_LIMIT_SYMBOLS: &[&str] = &[
    "bigcap",
    "bigcup",
    "bigodot",
    "bigoplus",
    "bigotimes",
    "bigsqcup",
    "biguplus",
    "bigvee",
    "bigwedge",
    "coprod",
    "int",
    "iint",
    "iiint",
    "oint",
    "prod",
    "sum",
];

const RELATION_COMMANDS: &[&str] = &[
    "Leftarrow",
    "Leftrightarrow",
    "Longleftarrow",
    "Longleftrightarrow",
    "Longrightarrow",
    "Rightarrow",
    "Join",
    "Vdash",
    "Vvdash",
    "approx",
    "asymp",
    "bowtie",
    "cong",
    "dashv",
    "fullouterjoin",
    "doteq",
    "downarrow",
    "equiv",
    "ge",
    "geq",
    "geqslant",
    "gets",
    "gg",
    "hookleftarrow",
    "hookrightarrow",
    "iff",
    "implies",
    "in",
    "leadsto",
    "le",
    "leftarrow",
    "leftharpoondown",
    "leftharpoonup",
    "leftrightarrow",
    "leftrightharpoons",
    "leftouterjoin",
    "leq",
    "leqslant",
    "ll",
    "longleftarrow",
    "longleftrightarrow",
    "longmapsto",
    "longrightarrow",
    "ltimes",
    "mapsto",
    "mid",
    "models",
    "ne",
    "nearrow",
    "neq",
    "ni",
    "notin",
    "nvdash",
    "nvDash",
    "nwarrow",
    "parallel",
    "perp",
    "prec",
    "preceq",
    "propto",
    "rightharpoondown",
    "rightharpoonup",
    "rightleftharpoons",
    "rightouterjoin",
    "rightarrow",
    "rightsquigarrow",
    "rtimes",
    "searrow",
    "sim",
    "simeq",
    "sqsubset",
    "sqsubseteq",
    "sqsupset",
    "sqsupseteq",
    "subset",
    "subseteq",
    "succ",
    "succeq",
    "supset",
    "supseteq",
    "swarrow",
    "to",
    "triangleleft",
    "triangleright",
    "twoheadleftarrow",
    "twoheadrightarrow",
    "uparrow",
    "vdash",
];

const SPACING_COMMANDS: &[&str] = &[
    ",",
    ":",
    ";",
    " ",
    ">",
    "enspace",
    "enskip",
    "medspace",
    "quad",
    "qquad",
    "thickspace",
    "thinspace",
];

const NEGATIVE_SPACING_COMMANDS: &[&str] = &["!", "negmedspace", "negthickspace", "negthinspace"];

const FONT_SWITCH_COMMANDS: &[&str] = &["bf", "cal", "it", "rm", "sf", "sl", "tt"];

const IGNORED_COMMANDS: &[&str] = &[
    "displaystyle",
    "limits",
    "nolimits",
    "scriptstyle",
    "scriptscriptstyle",
    "textstyle",
];

const SIZE_COMMANDS: &[&str] = &[
    "big", "Big", "bigg", "Bigg", "bigl", "Bigl", "biggl", "Biggl", "bigr", "Bigr", "biggr",
    "Biggr",
];

const PLAIN_WRAPPERS: &[&str] = &[
    "emph",
    "mathcal",
    "mathbf",
    "mathfrak",
    "mathit",
    "mathrm",
    "mathnormal",
    "mathscr",
    "mathsf",
    "mathtt",
    "mathup",
    "mbox",
    "overbrace",
    "pmb",
    "smash",
    "substack",
    "text",
    "textbf",
    "textit",
    "textmd",
    "textnormal",
    "textrm",
    "textsc",
    "textsf",
    "textsl",
    "texttt",
    "textup",
    "underbrace",
    "bm",
    "boldsymbol",
];

/// The markers that delimit a named operator in the intermediate rendering.
const NAMED_OPERATOR_START: char = '\u{f0004}';
const NAMED_OPERATOR_END: char = '\u{f0005}';
/// The markers that delimit a layout node in the intermediate rendering.
const LAYOUT_MARKER_START: char = '\u{f0000}';
const LAYOUT_MARKER_END: char = '\u{f0001}';
/// A space that survives the whitespace normalization of the intermediate rendering.
const PROTECTED_SPACE: char = '\u{f0002}';
/// The marker of a negative spacing command.
const NEGATIVE_SPACE: &str = "\u{0}";

/// The options of [`render_latex`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderLatexOptions {
    /// Stack fractions and operator limits vertically for display math (default: false).
    pub display: Option<bool>,
}

/// The kind of a script: a subscript or a superscript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
    Sub,
    Sup,
}

/// How an operator renders its lower limit when it is not stacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineLowerStyle {
    Bracket,
    Script,
}

/// A piece of the rendering that has its own lines.
#[derive(Debug, Clone)]
enum LayoutNode {
    Fraction {
        numerator: String,
        denominator: String,
    },
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
    Script {
        lower: Option<String>,
        upper: Option<String>,
    },
    Matrix {
        lines: Vec<String>,
        baseline: usize,
    },
}

/// A block of lines with its width and the index of its baseline line.
#[derive(Debug, Clone)]
struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

/// Whether a character is whitespace in JavaScript.
fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{9}'..='\u{d}'
            | '\u{20}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// The text without the leading whitespace.
fn trim_js_start(text: &str) -> &str {
    text.trim_start_matches(is_js_whitespace)
}

/// The text without the trailing whitespace.
fn trim_js_end(text: &str) -> &str {
    text.trim_end_matches(is_js_whitespace)
}

/// The text without the leading and trailing whitespace.
fn trim_js(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// The value of a table entry.
fn lookup<'a>(table: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table
        .iter()
        .find_map(|(name, value)| if *name == key { Some(*value) } else { None })
}

/// The value of a table entry whose key is a single character.
fn lookup_character<'a>(table: &'a [(&'a str, &'a str)], key: char) -> Option<&'a str> {
    table.iter().find_map(|(name, value)| {
        let mut characters = name.chars();
        match (characters.next(), characters.next()) {
            (Some(name), None) if name == key => Some(*value),
            _ => None,
        }
    })
}

/// Whether a table holds a key.
fn contains(table: &[&str], key: &str) -> bool {
    table.contains(&key)
}

/// The text with every character replaced by its table entry, or `None` when a character has none.
fn replace_characters(value: &str, replacements: &[(&str, &str)]) -> Option<String> {
    let mut result = String::with_capacity(value.len());
    for character in value.chars() {
        result.push_str(lookup_character(replacements, character)?);
    }
    Some(result)
}

/// The script value without the whitespace around its operators.
fn normalize_script_value(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    let mut result = String::with_capacity(value.len());
    let mut index = 0;
    while index < characters.len() {
        let mut operator = index;
        while operator < characters.len() && is_js_whitespace(characters[operator]) {
            operator += 1;
        }
        if operator < characters.len() && matches!(characters[operator], '=' | '+' | '-') {
            result.push(characters[operator]);
            let mut next = operator + 1;
            while next < characters.len() && is_js_whitespace(characters[next]) {
                next += 1;
            }
            index = next;
            continue;
        }
        result.push(characters[index]);
        index += 1;
    }
    result
}

/// The script value as Unicode super- or subscripts, or `None` when a character has no script.
fn format_unicode_script(value: &str, kind: ScriptKind) -> Option<String> {
    let normalized = normalize_script_value(value);
    let table = match kind {
        ScriptKind::Sub => SUBSCRIPTS,
        ScriptKind::Sup => SUPERSCRIPTS,
    };
    replace_characters(&normalized, table)
}

/// The script value as a Unicode script or as a marked value.
fn format_script(value: &str, kind: ScriptKind) -> String {
    let value = normalize_script_value(value);
    if let Some(unicode) = format_unicode_script(&value, kind) {
        return unicode;
    }
    let prefix = match kind {
        ScriptKind::Sub => "_",
        ScriptKind::Sup => "^",
    };
    let is_letters = !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphabetic());
    if value.chars().count() == 1 || (kind == ScriptKind::Sub && is_letters) {
        return format!("{prefix}{value}");
    }
    format!("{prefix}({value})")
}

/// The fraction as an inline division.
fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = trim_js(numerator);
    let denominator = trim_js(denominator);
    let simple_numerator = !numerator.is_empty()
        && numerator.chars().all(|character| {
            character.is_alphabetic() || character.is_numeric() || character == '.'
        });
    let simple_denominator = (!denominator.is_empty()
        && denominator
            .chars()
            .all(|character| character.is_numeric() || character == '.'))
        || denominator.chars().count() == 1;
    let numerator = if simple_numerator {
        numerator.to_string()
    } else {
        format!("({numerator})")
    };
    let denominator = if simple_denominator {
        denominator.to_string()
    } else {
        format!("({denominator})")
    };
    format!("{numerator}/{denominator}")
}

/// The root as an inline radical.
fn format_root(value: &str, symbol: &str) -> String {
    let value = trim_js(value);
    let simple = !value.is_empty()
        && value.chars().all(|character| {
            character.is_alphabetic() || character.is_numeric() || character == '.'
        });
    if simple {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

/// Whether a character ends a named operator with a space before it.
fn is_named_operator_left_context(character: char) -> bool {
    character.is_alphabetic()
        || character.is_numeric()
        || matches!(character, ')' | ']' | '}')
        || character == LAYOUT_MARKER_END
}

/// Whether a character starts a named operator with a space after it.
fn is_named_operator_right_context(character: char) -> bool {
    character.is_alphabetic()
        || character.is_numeric()
        || character == '√'
        || character == LAYOUT_MARKER_START
}

/// The text with the runs of spaces and tabs collapsed.
fn collapse_spaces(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut in_run = false;
    for character in line.chars() {
        if character == ' ' || character == '\t' {
            if !in_run {
                result.push(' ');
                in_run = true;
            }
        } else {
            result.push(character);
            in_run = false;
        }
    }
    result
}

/// The rendered text with its named operators spaced and its whitespace normalized.
fn normalize_output(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    let mut replaced = String::with_capacity(value.len());
    for (index, &character) in characters.iter().enumerate() {
        match character {
            NAMED_OPERATOR_START => {
                if index > 0 && is_named_operator_left_context(characters[index - 1]) {
                    replaced.push(' ');
                }
            }
            NAMED_OPERATOR_END => {
                if let Some(&next) = characters.get(index + 1)
                    && is_named_operator_right_context(next)
                {
                    replaced.push(' ');
                }
            }
            _ => replaced.push(character),
        }
    }

    let lines: Vec<String> = replaced
        .split('\n')
        .map(|line| trim_js(&collapse_spaces(line)).to_string())
        .collect();
    let kept: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| !line.is_empty() || (*index > 0 && *index < lines.len() - 1))
        .map(|(_, line)| line.clone())
        .collect();
    trim_js(&kept.join("\n")).to_string()
}

/// The line padded to the given width.
fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    let mut result = String::with_capacity(width + line.len());
    result.push_str(&" ".repeat(left));
    result.push_str(line);
    result.push_str(&" ".repeat(padding - left));
    result
}

/// The layouts side by side.
fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts
        .iter()
        .map(|layout| layout.baseline)
        .max()
        .unwrap_or(0);
    let below = layouts
        .iter()
        .map(|layout| layout.lines.len().saturating_sub(layout.baseline + 1))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::with_capacity(baseline + below + 1);
    for row in 0..=(baseline + below) {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row as isize - baseline as isize + layout.baseline as isize;
            if source_row >= 0
                && let Some(text) = layout.lines.get(source_row as usize)
            {
                line.push_str(&pad_layout_line(text, layout.width, false));
                continue;
            }
            line.push_str(&" ".repeat(layout.width));
        }
        lines.push(trim_js_end(&line).to_string());
    }
    let width = layouts.iter().map(|layout| layout.width).sum();
    Layout {
        lines,
        width,
        baseline,
    }
}

/// The index of the layout node a marker points at, and the index after the marker.
fn parse_layout_marker(characters: &[char], start: usize) -> Option<(usize, usize)> {
    let mut index = start + 1;
    while index < characters.len() && characters[index].is_ascii_digit() {
        index += 1;
    }
    if index == start + 1 || index >= characters.len() || characters[index] != LAYOUT_MARKER_END {
        return None;
    }
    let digits: String = characters[start + 1..index].iter().collect();
    let node = digits.parse::<usize>().ok()?;
    Some((node, index + 1))
}

/// The index of the layout node a trailing marker points at.
fn trailing_layout_marker(value: &str) -> Option<usize> {
    let characters: Vec<char> = value.chars().collect();
    if characters.len() < 3 || characters[characters.len() - 1] != LAYOUT_MARKER_END {
        return None;
    }
    let end = characters.len() - 1;
    let mut start = end;
    while start > 0 && characters[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == end || characters[start - 1] != LAYOUT_MARKER_START {
        return None;
    }
    let digits: String = characters[start..end].iter().collect();
    digits.parse::<usize>().ok()
}

/// The lines of a layout node.
fn render_node(node: &LayoutNode, nodes: &[LayoutNode]) -> Layout {
    match node {
        LayoutNode::Fraction {
            numerator,
            denominator,
        } => {
            let numerator = render_layout(numerator, nodes);
            let denominator = render_layout(denominator, nodes);
            let content_width = numerator.width.max(denominator.width).max(1);
            let width = content_width + 2;
            let mut lines: Vec<String> = numerator
                .lines
                .iter()
                .map(|line| pad_layout_line(line, width, true))
                .collect();
            lines.push(format!(" {} ", "─".repeat(content_width)));
            lines.extend(
                denominator
                    .lines
                    .iter()
                    .map(|line| pad_layout_line(line, width, true)),
            );
            Layout {
                lines,
                width,
                baseline: numerator.lines.len(),
            }
        }
        LayoutNode::Operator {
            operator,
            lower,
            upper,
        } => {
            let content_width = visible_width(operator)
                .max(lower.as_deref().map_or(0, visible_width))
                .max(upper.as_deref().map_or(0, visible_width));
            let mut lines = Vec::new();
            if let Some(upper) = upper {
                lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
            }
            lines.push(format!(
                "{} ",
                pad_layout_line(operator, content_width, true)
            ));
            if let Some(lower) = lower {
                lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
            }
            Layout {
                lines,
                width: content_width + 1,
                baseline: usize::from(upper.is_some()),
            }
        }
        LayoutNode::Script { lower, upper } => {
            let upper = upper.as_deref().map(|value| render_layout(value, nodes));
            let lower = lower.as_deref().map(|value| render_layout(value, nodes));
            let width = upper
                .as_ref()
                .map_or(0, |layout| layout.width)
                .max(lower.as_ref().map_or(0, |layout| layout.width));
            let mut lines = Vec::new();
            if let Some(upper) = &upper {
                lines.extend(
                    upper
                        .lines
                        .iter()
                        .map(|line| pad_layout_line(line, width, false)),
                );
            }
            lines.push(" ".repeat(width));
            if let Some(lower) = &lower {
                lines.extend(
                    lower
                        .lines
                        .iter()
                        .map(|line| pad_layout_line(line, width, false)),
                );
            }
            Layout {
                lines,
                width,
                baseline: upper.as_ref().map_or(0, |layout| layout.lines.len()),
            }
        }
        LayoutNode::Matrix { lines, baseline } => {
            let width = lines
                .iter()
                .map(|line| visible_width(line))
                .max()
                .unwrap_or(0);
            Layout {
                lines: lines
                    .iter()
                    .map(|line| pad_layout_line(line, width, false))
                    .collect(),
                width,
                baseline: *baseline,
            }
        }
    }
}

/// The rendered text with its layout nodes drawn.
fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0;
    for source_line in source.split('\n') {
        let characters: Vec<char> = source_line.chars().collect();
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0;
        let mut previous_node: Option<&LayoutNode> = None;
        let mut scan = 0;
        while scan < characters.len() {
            if characters[scan] == LAYOUT_MARKER_START
                && let Some((node_index, end)) = parse_layout_marker(&characters, scan)
            {
                if let Some(node) = nodes.get(node_index) {
                    if scan > position {
                        let sliced: String = characters[position..scan].iter().collect();
                        let trimmed = trim_js_end(if previous_node.is_some() {
                            trim_js_start(&sliced)
                        } else {
                            sliced.as_str()
                        });
                        let preserve_leading =
                            matches!(previous_node, Some(LayoutNode::Matrix { .. }))
                                && sliced.chars().next().is_some_and(is_js_whitespace);
                        let preserve_trailing = matches!(node, LayoutNode::Matrix { .. })
                            && sliced.chars().last().is_some_and(is_js_whitespace);
                        let text = build_text(trimmed, preserve_leading, preserve_trailing);
                        layouts.push(Layout {
                            lines: vec![text.clone()],
                            width: visible_width(&text),
                            baseline: 0,
                        });
                    }
                    layouts.push(render_node(node, nodes));
                    position = end;
                    previous_node = Some(node);
                }
                scan = end;
                continue;
            }
            scan += 1;
        }
        if position < characters.len() {
            let sliced: String = characters[position..].iter().collect();
            let trimmed = if previous_node.is_some() {
                trim_js_start(&sliced)
            } else {
                sliced.as_str()
            };
            let text = if matches!(previous_node, Some(LayoutNode::Matrix { .. }))
                && sliced.chars().next().is_some_and(is_js_whitespace)
            {
                format!(" {trimmed}")
            } else {
                trimmed.to_string()
            };
            layouts.push(Layout {
                lines: vec![text.clone()],
                width: visible_width(&text),
                baseline: 0,
            });
        }
        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }
    let width = rendered_lines
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or(0);
    Layout {
        lines: rendered_lines,
        width,
        baseline: first_baseline,
    }
}

/// The text with the preserved spaces around it.
fn build_text(trimmed: &str, preserve_leading: bool, preserve_trailing: bool) -> String {
    if !trimmed.is_empty() {
        let mut text = String::with_capacity(trimmed.len() + 2);
        if preserve_leading {
            text.push(' ');
        }
        text.push_str(trimmed);
        if preserve_trailing {
            text.push(' ');
        }
        text
    } else if preserve_leading || preserve_trailing {
        " ".to_string()
    } else {
        String::new()
    }
}

/// The parser of a LaTeX expression.
struct LatexParser<'a> {
    source: &'a [char],
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
    script_depth: usize,
}

impl<'a> LatexParser<'a> {
    /// A parser of the given source.
    fn new(source: &'a [char], display: bool) -> Self {
        Self {
            source,
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
            script_depth: 0,
        }
    }

    /// The rendered source, or `None` when the source holds unsupported syntax.
    fn render(&mut self, nodes: &mut Vec<LayoutNode>) -> Option<String> {
        let rendered = self.parse_sequence(nodes, None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    /// The parsed sequence, up to the end character when there is one.
    fn parse_sequence(
        &mut self,
        nodes: &mut Vec<LayoutNode>,
        end_character: Option<char>,
    ) -> String {
        let mut result = String::new();
        while self.position < self.source.len() {
            let character = self.source[self.position];
            if end_character == Some(character) {
                self.position += 1;
                return result;
            }
            if character == '}' {
                self.supported = false;
                return result;
            }
            if character == '{' {
                self.position += 1;
                let inner = self.parse_sequence(nodes, Some('}'));
                result.push_str(&inner);
                continue;
            }
            if character == '\\' {
                let command = self.parse_command(nodes);
                if command == NEGATIVE_SPACE {
                    result = trim_js_end(&result).to_string();
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.pop();
                    }
                } else {
                    result.push_str(&command);
                }
                continue;
            }
            if character == '^' || character == '_' {
                self.position += 1;
                result = trim_js_end(&result).to_string();
                let script = self.parse_scripts(nodes, character);
                if result.ends_with(NAMED_OPERATOR_END) {
                    let mut updated =
                        result[..result.len() - NAMED_OPERATOR_END.len_utf8()].to_string();
                    updated.push_str(&script);
                    updated.push(NAMED_OPERATOR_END);
                    result = updated;
                } else {
                    result.push_str(&script);
                }
                continue;
            }
            if is_js_whitespace(character) {
                result.push_str(&self.parse_whitespace());
                continue;
            }
            if character == '=' || character == '<' || character == '>' {
                result = format!("{} {character} ", trim_js_end(&result));
                self.position += 1;
                continue;
            }
            if character == '&' {
                self.position += 1;
                continue;
            }
            if character == '~' {
                self.position += 1;
                result.push(' ');
                continue;
            }
            if character == '.'
                && let Some(node_index) = trailing_layout_marker(&result)
                && let Some(LayoutNode::Matrix { lines, .. }) = nodes.get_mut(node_index)
                && let Some(last) = lines.last_mut()
            {
                last.push('.');
                self.position += 1;
                continue;
            }
            result.push(character);
            self.position += 1;
        }
        if end_character.is_some() {
            self.supported = false;
        }
        result
    }

    /// The scripts of a script marker.
    fn parse_scripts(&mut self, nodes: &mut Vec<LayoutNode>, initial_marker: char) -> String {
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        let mut order: Vec<ScriptKind> = Vec::new();

        let first = self.parse_script_argument(nodes);
        match initial_marker {
            '_' => {
                sub = Some(first);
                order.push(ScriptKind::Sub);
            }
            _ => {
                sup = Some(first);
                order.push(ScriptKind::Sup);
            }
        }

        let mut next_position = self.position;
        while next_position < self.source.len() && is_js_whitespace(self.source[next_position]) {
            next_position += 1;
        }
        if let Some(next_marker) = self.source.get(next_position).copied()
            && matches!(next_marker, '^' | '_')
            && next_marker != initial_marker
        {
            self.position = next_position + 1;
            let second = self.parse_script_argument(nodes);
            match next_marker {
                '_' => {
                    sub = Some(second);
                    order.push(ScriptKind::Sub);
                }
                _ => {
                    sup = Some(second);
                    order.push(ScriptKind::Sup);
                }
            }
        }

        let sub_unicode = sub
            .as_deref()
            .and_then(|value| format_unicode_script(value, ScriptKind::Sub));
        let sup_unicode = sup
            .as_deref()
            .and_then(|value| format_unicode_script(value, ScriptKind::Sup));
        let can_use_layout = [sub.as_deref(), sup.as_deref()]
            .iter()
            .all(|value| match value {
                None => true,
                Some(value) => {
                    !(value.contains('/')
                        || (!value.contains(LAYOUT_MARKER_START)
                            && value.chars().count() > 1
                            && !value.chars().any(|character| {
                                character.is_ascii_uppercase()
                                    || character == '*'
                                    || character == '∗'
                            })))
                }
            });
        let needs_layout = self.display
            && can_use_layout
            && (self.script_depth > 0
                || (sub.is_some() && sub_unicode.is_none())
                || (sup.is_some() && sup_unicode.is_none()));
        if !needs_layout {
            let mut rendered = String::new();
            for kind in order {
                let (value, unicode) = match kind {
                    ScriptKind::Sub => (sub.clone().unwrap_or_default(), sub_unicode.clone()),
                    ScriptKind::Sup => (sup.clone().unwrap_or_default(), sup_unicode.clone()),
                };
                rendered.push_str(&unicode.unwrap_or_else(|| format_script(&value, kind)));
            }
            return rendered;
        }

        let index = nodes.len();
        nodes.push(LayoutNode::Script {
            lower: sub.as_deref().map(normalize_output),
            upper: sup.as_deref().map(normalize_output),
        });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    /// The argument of a script, with the script depth counted.
    fn parse_script_argument(&mut self, nodes: &mut Vec<LayoutNode>) -> String {
        self.script_depth += 1;
        let value = self.parse_required_argument(nodes, false);
        self.script_depth -= 1;
        value
    }

    /// The whitespace run as a single space.
    fn parse_whitespace(&mut self) -> String {
        while self.position < self.source.len() && is_js_whitespace(self.source[self.position]) {
            self.position += 1;
        }
        " ".to_string()
    }

    /// The command and its arguments.
    fn parse_command(&mut self, nodes: &mut Vec<LayoutNode>) -> String {
        self.position += 1;
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }

        let first = self.source[self.position];
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.source.get(self.position) == Some(&'\n') {
                self.position += 1;
            }
            return " ".to_string();
        }
        let command = if first.is_ascii_alphabetic() {
            let start = self.position;
            while self.position < self.source.len()
                && self.source[self.position].is_ascii_alphabetic()
            {
                self.position += 1;
            }
            self.source[start..self.position].iter().collect()
        } else {
            self.position += 1;
            first.to_string()
        };

        if command == "\\" {
            return "\n".to_string();
        }
        if contains(SPACING_COMMANDS, &command) {
            return " ".to_string();
        }
        if contains(NEGATIVE_SPACING_COMMANDS, &command) {
            return NEGATIVE_SPACE.to_string();
        }
        if contains(FONT_SWITCH_COMMANDS, &command) {
            while self.position < self.source.len() && is_js_whitespace(self.source[self.position])
            {
                self.position += 1;
            }
            return String::new();
        }
        if contains(IGNORED_COMMANDS, &command) {
            return String::new();
        }
        if matches!(command.as_str(), "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command;
        }
        if command == "|" {
            return "‖".to_string();
        }
        if command == "not" {
            let value = trim_js(&self.parse_required_argument(nodes, false)).to_string();
            if let Some(negated) = lookup(NEGATED_SYMBOLS, &value) {
                return format!(" {negated} ");
            }
            let characters: Vec<char> = value.chars().collect();
            let Some(&first) = characters.first() else {
                self.supported = false;
                return String::new();
            };
            let rest: String = characters[1..].iter().collect();
            return format!(" {first}\u{338}{rest} ");
        }
        if contains(LIMIT_OPERATORS, &command) {
            return self.parse_operator(nodes, &command, InlineLowerStyle::Bracket, true, true);
        }

        if let Some(symbol) = lookup(SYMBOLS, &command) {
            if contains(DISPLAY_LIMIT_SYMBOLS, &command) {
                return self.parse_operator(nodes, symbol, InlineLowerStyle::Script, true, false);
            }
            if command == "cdot" || command == "times" || contains(RELATION_COMMANDS, &command) {
                return format!(" {symbol} ");
            }
            return symbol.to_string();
        }
        if contains(NAMED_OPERATORS, &command) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if contains(SIZE_COMMANDS, &command) {
            return String::new();
        }
        if matches!(command.as_str(), "left" | "middle" | "right") {
            if self.source.get(self.position) == Some(&'.') {
                self.position += 1;
            }
            return String::new();
        }
        if matches!(command.as_str(), "frac" | "dfrac" | "tfrac") {
            let should_stack = self.display && self.stack_fractions && command != "tfrac";
            let numerator = self.parse_required_argument(nodes, !should_stack);
            let denominator = self.parse_required_argument(nodes, !should_stack);
            if should_stack {
                let index = nodes.len();
                nodes.push(LayoutNode::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
                return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
            }
            return format_fraction(&numerator, &denominator);
        }
        if command == "sqrt" {
            let degree = self
                .parse_optional_argument(nodes)
                .map(|value| trim_js(&value).to_string());
            let value = self.parse_required_argument(nodes, true);
            return match degree.as_deref() {
                None | Some("2") => format_root(&value, "√"),
                Some("3") => format_root(&value, "∛"),
                Some("4") => format_root(&value, "∜"),
                Some(degree) => format!(
                    "{}{}",
                    format_script(degree, ScriptKind::Sup),
                    format_root(&value, "√")
                ),
            };
        }
        if matches!(command.as_str(), "boxed" | "fbox") {
            let value = self.parse_required_argument(nodes, true);
            return format!("[{}]", trim_js(&value));
        }
        if matches!(command.as_str(), "binom" | "dbinom" | "tbinom") {
            let numerator = self.parse_required_argument(nodes, true);
            let denominator = self.parse_required_argument(nodes, true);
            return format!("({numerator} choose {denominator})");
        }
        if let Some(accent) = lookup(ACCENTS, &command) {
            let value = self.parse_required_argument(nodes, true);
            if value.chars().count() == 1 {
                return format!("{value}{accent}");
            }
            return format!("{command}({value})");
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(nodes, true);
            let mut result = String::with_capacity(value.len());
            for character in value.chars() {
                match lookup_character(BLACKBOARD, character) {
                    Some(replacement) => result.push_str(replacement),
                    None => result.push(character),
                }
            }
            return result;
        }
        if command == "operatorname" {
            let starred = self.source.get(self.position) == Some(&'*');
            if starred {
                self.position += 1;
            }
            let operator = trim_js(&normalize_output(
                &self.parse_required_argument(nodes, true),
            ))
            .to_string();
            return self.parse_operator(nodes, &operator, InlineLowerStyle::Bracket, starred, true);
        }
        if matches!(command.as_str(), "mod" | "bmod") {
            return " mod ".to_string();
        }
        if matches!(command.as_str(), "pmod" | "pod") {
            let value = trim_js(&self.parse_required_argument(nodes, true)).to_string();
            if command == "pmod" {
                return format!(" (mod {value})");
            }
            return format!(" ({value})");
        }
        if matches!(command.as_str(), "overset" | "stackrel") {
            let upper = self.parse_required_argument(nodes, true);
            let value = trim_js(&self.parse_required_argument(nodes, true)).to_string();
            return format!("{value}{}", format_script(&upper, ScriptKind::Sup));
        }
        if command == "underset" {
            let lower = self.parse_required_argument(nodes, true);
            let value = trim_js(&self.parse_required_argument(nodes, true)).to_string();
            return format!("{value}{}", format_script(&lower, ScriptKind::Sub));
        }
        if contains(PLAIN_WRAPPERS, &command) {
            let value = self.parse_required_argument(nodes, true);
            if command.starts_with("text") || command == "mbox" {
                return value;
            }
            return trim_js(&value).to_string();
        }
        if command == "begin" {
            return self.parse_environment(nodes);
        }
        if command == "end" {
            self.supported = false;
            return String::new();
        }

        self.supported = false;
        format!("\\{command}")
    }

    /// The operator and its limits.
    fn parse_operator(
        &mut self,
        nodes: &mut Vec<LayoutNode>,
        operator: &str,
        inline_lower_style: InlineLowerStyle,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        let mut modifier_position = self.position;
        while modifier_position < self.source.len()
            && matches!(self.source[modifier_position], ' ' | '\t')
        {
            modifier_position += 1;
        }
        if let Some((is_limits, length)) = parse_limits_modifier(&self.source[modifier_position..])
        {
            use_display_limits = is_limits;
            self.position = modifier_position + length;
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while script_position < self.source.len()
                && matches!(self.source[script_position], ' ' | '\t')
            {
                script_position += 1;
            }
            let Some(kind) = self.source.get(script_position).copied() else {
                break;
            };
            if kind != '_' && kind != '^' {
                break;
            }
            self.position = script_position + 1;
            let value =
                normalize_output(&self.parse_required_argument(nodes, false)).replace(' ', "");
            if kind == '_' {
                if lower.is_some() {
                    self.supported = false;
                }
                lower = Some(value);
            } else {
                if upper.is_some() {
                    self.supported = false;
                }
                upper = Some(value);
            }
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            let index = nodes.len();
            nodes.push(LayoutNode::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
            return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
        }

        let mut rendered = operator.to_string();
        if let Some(lower) = lower {
            rendered.push_str(&match inline_lower_style {
                InlineLowerStyle::Bracket => format!("[{lower}]"),
                InlineLowerStyle::Script => format_script(&lower, ScriptKind::Sub),
            });
        }
        if let Some(upper) = upper {
            rendered.push_str(&format_script(&upper, ScriptKind::Sup));
        }
        if spaced {
            format!(" {rendered} ")
        } else {
            rendered
        }
    }

    /// A required argument.
    fn parse_required_argument(
        &mut self,
        nodes: &mut Vec<LayoutNode>,
        stack_fractions: bool,
    ) -> String {
        let previous_stack_fractions = self.stack_fractions;
        self.stack_fractions = previous_stack_fractions && stack_fractions;
        let value = self.parse_required_argument_value(nodes);
        self.stack_fractions = previous_stack_fractions;
        value
    }

    /// The value of a required argument.
    fn parse_required_argument_value(&mut self, nodes: &mut Vec<LayoutNode>) -> String {
        while self.position < self.source.len() && is_js_whitespace(self.source[self.position]) {
            self.position += 1;
        }
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }
        if self.source[self.position] == '{' {
            self.position += 1;
            return self.parse_sequence(nodes, Some('}'));
        }
        if self.source[self.position] == '\\' {
            return self.parse_command(nodes);
        }
        let value = self.source[self.position];
        self.position += 1;
        value.to_string()
    }

    /// An optional argument, in square brackets.
    fn parse_optional_argument(&mut self, nodes: &mut Vec<LayoutNode>) -> Option<String> {
        while self.position < self.source.len() && matches!(self.source[self.position], ' ' | '\t')
        {
            self.position += 1;
        }
        if self.source.get(self.position) != Some(&'[') {
            return None;
        }
        let end = self.source[self.position + 1..]
            .iter()
            .position(|character| *character == ']')
            .map(|offset| self.position + 1 + offset);
        let Some(end) = end else {
            self.supported = false;
            return None;
        };
        let value: String = self.source[self.position + 1..end].iter().collect();
        self.position = end + 1;
        Some(self.render_nested(nodes, &value, true))
    }

    /// A group, without parsing its content.
    fn read_raw_group(&mut self) -> Option<String> {
        while self.position < self.source.len() && matches!(self.source[self.position], ' ' | '\t')
        {
            self.position += 1;
        }
        if self.source.get(self.position) != Some(&'{') {
            self.supported = false;
            return None;
        }

        self.position += 1;
        let start = self.position;
        let mut depth = 1;
        while self.position < self.source.len() {
            let character = self.source[self.position];
            if character == '\\' {
                self.position += 2;
                continue;
            }
            if character == '{' {
                depth += 1;
            }
            if character == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value: String = self.source[start..self.position].iter().collect();
                self.position += 1;
                return Some(value);
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    /// An environment and its body.
    fn parse_environment(&mut self, nodes: &mut Vec<LayoutNode>) -> String {
        let Some(environment) = self.read_raw_group() else {
            return String::new();
        };
        let end_marker = format!("\\end{{{environment}}}");
        let Some(end) = find_substring(self.source, self.position, &end_marker) else {
            self.supported = false;
            return String::new();
        };
        let body: String = self.source[self.position..end].iter().collect();
        self.position = end + end_marker.chars().count();

        if matches!(
            environment.as_str(),
            "equation" | "equation*" | "displaymath"
        ) {
            return trim_js(&self.render_nested(nodes, &body, true)).to_string();
        }

        if matches!(
            environment.as_str(),
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            let aligned_at = matches!(environment.as_str(), "alignedat" | "alignat" | "alignat*");
            let aligned_body = if aligned_at {
                strip_leading_group(&body)
            } else {
                body.clone()
            };
            let rows: Vec<String> = split_environment_rows(&aligned_body)
                .iter()
                .map(|row| {
                    let cells: Vec<&str> = row.split('&').collect();
                    let source = if aligned_at {
                        let pairs = cells.len().div_ceil(2);
                        (0..pairs)
                            .map(|index| {
                                let start = index * 2;
                                let end = (start + 2).min(cells.len());
                                cells[start..end].concat()
                            })
                            .collect::<Vec<String>>()
                            .join(" ")
                    } else {
                        cells.concat()
                    };
                    trim_js(&self.render_nested(nodes, &source, true)).to_string()
                })
                .filter(|row| !row.is_empty())
                .collect();
            return rows.join("\n");
        }

        if matches!(environment.as_str(), "cases" | "cases*") {
            return self.render_cases(nodes, &body);
        }

        if matches!(
            environment.as_str(),
            "array"
                | "matrix"
                | "smallmatrix"
                | "pmatrix"
                | "bmatrix"
                | "Bmatrix"
                | "vmatrix"
                | "Vmatrix"
        ) {
            let matrix_body = if environment == "array" {
                strip_leading_group(&body)
            } else {
                body.clone()
            };
            return self.render_matrix(nodes, &environment, &matrix_body);
        }

        self.supported = false;
        body
    }

    /// The rows of a cases environment.
    fn render_cases(&mut self, nodes: &mut Vec<LayoutNode>, body: &str) -> String {
        let rows: Vec<Vec<String>> = split_environment_rows(body)
            .iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| trim_js(&self.render_nested(nodes, cell, false)).to_string())
                    .collect::<Vec<String>>()
            })
            .filter(|row| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        let value_width = rows
            .iter()
            .map(|row| {
                visible_width(&strip_trailing_comma(
                    row.first().map_or("", String::as_str),
                ))
            })
            .max()
            .unwrap_or(0);
        let contents: Vec<String> = rows
            .iter()
            .map(|row| {
                let value = strip_trailing_comma(row.first().map_or("", String::as_str));
                let condition = row.get(1).map_or("", String::as_str);
                if condition.is_empty() {
                    return value;
                }
                let prefix = if starts_with_condition_keyword(condition) {
                    " "
                } else {
                    " if "
                };
                let padding = value_width.saturating_sub(visible_width(&value));
                format!(
                    "{value}{}{prefix}{condition}",
                    PROTECTED_SPACE.to_string().repeat(padding)
                )
            })
            .collect();
        if contents.len() <= 1 {
            return match contents.first() {
                None => String::new(),
                Some(content) => format!("⎧ {content}"),
            };
        }

        let middle = contents.len() / 2;
        let mut visual_rows: Vec<Option<String>> = contents.into_iter().map(Some).collect();
        if visual_rows.len().is_multiple_of(2) {
            visual_rows.insert(middle, None);
        }
        let lines: Vec<String> = visual_rows
            .iter()
            .enumerate()
            .map(|(index, content)| {
                let delimiter = if index == 0 {
                    "⎧"
                } else if index == visual_rows.len() - 1 {
                    "⎩"
                } else {
                    "⎨"
                };
                match content {
                    None => delimiter.to_string(),
                    Some(content) => format!("{delimiter} {content}"),
                }
            })
            .collect();
        let index = nodes.len();
        nodes.push(LayoutNode::Matrix {
            lines,
            baseline: middle,
        });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    /// The rows of a matrix environment.
    fn render_matrix(
        &mut self,
        nodes: &mut Vec<LayoutNode>,
        environment: &str,
        body: &str,
    ) -> String {
        let matrix: Vec<Vec<String>> = split_environment_rows(body)
            .iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| trim_js(&self.render_nested(nodes, cell, false)).to_string())
                    .collect::<Vec<String>>()
            })
            .filter(|row| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        let column_count = matrix.iter().map(|row| row.len()).max().unwrap_or(0);
        let column_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                matrix
                    .iter()
                    .map(|row| visible_width(row.get(column).map_or("", String::as_str)))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..column_count)
                    .map(|column| {
                        let cell = row.get(column).map_or("", String::as_str);
                        let padding = column_widths[column].saturating_sub(visible_width(cell));
                        format!("{cell}{}", PROTECTED_SPACE.to_string().repeat(padding))
                    })
                    .collect::<Vec<String>>()
                    .join(" │ ")
            })
            .collect();

        let lines: Vec<String> = if matches!(environment, "array" | "matrix" | "smallmatrix") {
            rows
        } else {
            let delimiter = match environment {
                "pmatrix" => ["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"],
                "bmatrix" => ["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"],
                "Bmatrix" => ["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"],
                "vmatrix" => ["│", "│", "│", "│", "│", "│"],
                "Vmatrix" => ["║", "║", "║", "║", "║", "║"],
                _ => {
                    self.supported = false;
                    return rows.join("\n");
                }
            };
            rows.iter()
                .enumerate()
                .map(|(index, row)| {
                    let left = if index == 0 {
                        delimiter[0]
                    } else if index == rows.len() - 1 {
                        delimiter[4]
                    } else {
                        delimiter[2]
                    };
                    let right = if index == 0 {
                        delimiter[1]
                    } else if index == rows.len() - 1 {
                        delimiter[5]
                    } else {
                        delimiter[3]
                    };
                    format!("{left} {row} {right}")
                })
                .collect()
        };

        if lines.len() <= 1 {
            return lines.first().cloned().unwrap_or_default();
        }
        let index = nodes.len();
        nodes.push(LayoutNode::Matrix { lines, baseline: 0 });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    /// The rendered source, parsed by a nested parser.
    fn render_nested(
        &mut self,
        nodes: &mut Vec<LayoutNode>,
        source: &str,
        stack_fractions: bool,
    ) -> String {
        let characters: Vec<char> = source.chars().collect();
        let mut nested = LatexParser::new(&characters, self.display && stack_fractions);
        match nested.render(nodes) {
            Some(rendered) => rendered,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}

/// The limits modifier that follows an operator, and its length.
fn parse_limits_modifier(characters: &[char]) -> Option<(bool, usize)> {
    if characters.first() != Some(&'\\') {
        return None;
    }
    let rest = &characters[1..];
    let is_limits = rest.starts_with(&['l', 'i', 'm', 'i', 't', 's']);
    let is_nolimits = rest.starts_with(&['n', 'o', 'l', 'i', 'm', 'i', 't', 's']);
    let length = if is_limits {
        6
    } else if is_nolimits {
        8
    } else {
        return None;
    };
    match characters.get(1 + length) {
        Some(character) if character.is_ascii_alphabetic() => None,
        _ => Some((is_limits, 1 + length)),
    }
}

/// The index of the needle in the haystack, at or after the given index.
fn find_substring(haystack: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
        .map(|position| position + from)
}

/// The rows of an environment body.
fn split_environment_rows(body: &str) -> Vec<String> {
    let characters: Vec<char> = body.chars().collect();
    let mut rows = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\\' && characters.get(index + 1) == Some(&'\\') {
            let mut end = index + 2;
            if characters.get(end) == Some(&'[') {
                let mut scan = end + 1;
                while scan < characters.len() && characters[scan] != ']' && characters[scan] != '\n'
                {
                    scan += 1;
                }
                if characters.get(scan) == Some(&']') {
                    end = scan + 1;
                }
            }
            rows.push(characters[start..index].iter().collect());
            start = end;
            index = end;
            continue;
        }
        index += 1;
    }
    rows.push(characters[start..].iter().collect());
    rows
}

/// The body without its leading group.
fn strip_leading_group(body: &str) -> String {
    let characters: Vec<char> = body.chars().collect();
    let mut index = 0;
    while index < characters.len() && is_js_whitespace(characters[index]) {
        index += 1;
    }
    if characters.get(index) == Some(&'{')
        && let Some(offset) = characters[index..]
            .iter()
            .position(|character| *character == '}')
    {
        return characters[index + offset + 1..].iter().collect();
    }
    body.to_string()
}

/// The value without its trailing comma.
fn strip_trailing_comma(value: &str) -> String {
    match trim_js_end(value).strip_suffix(',') {
        Some(stripped) => stripped.to_string(),
        None => value.to_string(),
    }
}

/// Whether the condition starts with a condition keyword.
fn starts_with_condition_keyword(condition: &str) -> bool {
    let lower: String = condition.to_lowercase();
    ["if", "when", "for", "otherwise"].iter().any(|keyword| {
        if !lower.starts_with(keyword) {
            return false;
        }
        match lower.as_bytes().get(keyword.len()) {
            None => true,
            Some(byte) => !(byte.is_ascii_alphanumeric() || *byte == b'_'),
        }
    })
}

/// Render a basic LaTeX math expression as terminal-friendly Unicode text.
/// Returns `None` when the expression contains unsupported or malformed syntax.
pub fn render_latex(source: &str, options: &RenderLatexOptions) -> Option<String> {
    let display = options.display.unwrap_or(false);
    let characters: Vec<char> = source.chars().collect();
    let mut nodes: Vec<LayoutNode> = Vec::new();
    let mut parser = LatexParser::new(&characters, display);
    let rendered = parser.render(&mut nodes)?;
    if nodes.is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }
    let lines = render_layout(&rendered, &nodes).lines;
    let indentation = lines
        .iter()
        .filter(|line| !trim_js(line).is_empty())
        .map(|line| line.chars().count() - trim_js_start(line).chars().count())
        .min()
        .unwrap_or(usize::MAX);
    let rendered = lines
        .iter()
        .map(|line| {
            let text: String = line.chars().skip(indentation).collect();
            trim_js_end(&text).to_string()
        })
        .collect::<Vec<String>>()
        .join("\n");
    Some(trim_js_end(&rendered).replace(PROTECTED_SPACE, " "))
}
