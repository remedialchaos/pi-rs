//! Upstream system theme recipe (families, contrast curves and draw surfaces).

#[derive(Clone, Copy)]
pub(super) struct Family {
    pub hue: f64,
    pub min: f64,
    pub max: f64,
    pub slot: u8,
}

const NEUTRAL: Family = Family {
    hue: 231.49,
    min: 0.02,
    max: 0.08,
    slot: 8,
};
const BLUE: Family = Family {
    hue: 231.49,
    min: 0.1,
    max: 0.68,
    slot: 4,
};
const GREEN: Family = Family {
    hue: 158.68,
    min: 0.1,
    max: 0.76,
    slot: 2,
};
const RED: Family = Family {
    hue: 20.0,
    min: 0.1,
    max: 0.92,
    slot: 1,
};
const YELLOW: Family = Family {
    hue: 82.36,
    min: 0.5,
    max: 1.0,
    slot: 3,
};
const ORANGE: Family = Family {
    hue: 52.0,
    min: 0.12,
    max: 0.85,
    slot: 3,
};
const VIOLET: Family = Family {
    hue: 295.0,
    min: 0.2,
    max: 0.6,
    slot: 5,
};
const CALAMINE: Family = Family {
    hue: 202.43,
    min: 0.1,
    max: 0.74,
    slot: 6,
};
const THINKINGSLATE: Family = Family {
    hue: 231.49,
    min: 0.08,
    max: 0.2,
    slot: 4,
};
const THINKINGBLUE: Family = Family {
    hue: 231.49,
    min: 0.2,
    max: 0.45,
    slot: 4,
};
const THINKINGPERIWINKLE: Family = Family {
    hue: 263.25,
    min: 0.3,
    max: 0.6,
    slot: 6,
};
const THINKINGVIOLET: Family = Family {
    hue: 295.0,
    min: 0.4,
    max: 0.75,
    slot: 5,
};
const THINKINGMAGENTA: Family = Family {
    hue: 337.5,
    min: 0.5,
    max: 0.85,
    slot: 13,
};
const THINKINGRED: Family = Family {
    hue: 20.0,
    min: 0.95,
    max: 1.0,
    slot: 1,
};

pub(super) const NEUTRAL_FAMILY: Family = NEUTRAL;

pub(super) fn families() -> impl ExactSizeIterator<Item = (&'static str, Family, u8, bool)> {
    const FAMILIES: &[(&str, Family, u8, bool)] = &[
        ("selectedBg", BLUE, BLUE.slot, false),
        ("searchMatchBg", ORANGE, 3, false),
        ("userMessageBg", BLUE, BLUE.slot, false),
        ("customMessageBg", VIOLET, VIOLET.slot, false),
        ("toolPendingBg", NEUTRAL, NEUTRAL.slot, true),
        ("toolSuccessBg", GREEN, GREEN.slot, false),
        ("toolErrorBg", RED, RED.slot, false),
        ("text", NEUTRAL, NEUTRAL.slot, true),
        ("userMessageText", NEUTRAL, NEUTRAL.slot, true),
        ("customMessageText", NEUTRAL, NEUTRAL.slot, true),
        ("toolTitle", NEUTRAL, NEUTRAL.slot, true),
        ("syntaxOperator", NEUTRAL, NEUTRAL.slot, true),
        ("syntaxPunctuation", NEUTRAL, NEUTRAL.slot, true),
        ("muted", NEUTRAL, NEUTRAL.slot, true),
        ("dim", NEUTRAL, NEUTRAL.slot, true),
        ("thinkingText", NEUTRAL, NEUTRAL.slot, true),
        ("toolOutput", NEUTRAL, NEUTRAL.slot, true),
        ("mdLinkUrl", NEUTRAL, NEUTRAL.slot, true),
        ("mdQuote", NEUTRAL, NEUTRAL.slot, true),
        ("mdQuoteBorder", NEUTRAL, NEUTRAL.slot, true),
        ("mdHr", NEUTRAL, NEUTRAL.slot, true),
        ("mdCodeBlockBorder", NEUTRAL, NEUTRAL.slot, true),
        ("toolDiffContext", NEUTRAL, NEUTRAL.slot, true),
        ("syntaxComment", NEUTRAL, NEUTRAL.slot, true),
        ("scrollbarTrack", NEUTRAL, NEUTRAL.slot, true),
        ("scrollbarThumb", NEUTRAL, NEUTRAL.slot, true),
        ("searchMatchText", NEUTRAL, NEUTRAL.slot, true),
        ("borderMuted", NEUTRAL, NEUTRAL.slot, true),
        ("accent", VIOLET, VIOLET.slot, false),
        ("borderAccent", VIOLET, VIOLET.slot, false),
        ("customMessageLabel", VIOLET, VIOLET.slot, false),
        ("mdCode", VIOLET, VIOLET.slot, false),
        ("mdListBullet", VIOLET, VIOLET.slot, false),
        ("syntaxType", VIOLET, VIOLET.slot, false),
        ("border", BLUE, BLUE.slot, false),
        ("mdLink", BLUE, BLUE.slot, false),
        ("syntaxKeyword", BLUE, BLUE.slot, false),
        ("syntaxVariable", CALAMINE, CALAMINE.slot, false),
        ("success", GREEN, GREEN.slot, false),
        ("mdCodeBlock", GREEN, GREEN.slot, false),
        ("toolDiffAdded", GREEN, GREEN.slot, false),
        ("bashMode", GREEN, GREEN.slot, false),
        ("syntaxNumber", GREEN, 5, false),
        ("error", RED, RED.slot, false),
        ("toolDiffRemoved", RED, RED.slot, false),
        ("warning", YELLOW, YELLOW.slot, false),
        ("mdHeading", YELLOW, YELLOW.slot, false),
        ("syntaxFunction", YELLOW, YELLOW.slot, false),
        ("syntaxString", ORANGE, 2, false),
        ("thinkingOff", NEUTRAL, NEUTRAL.slot, true),
        ("thinkingMinimal", THINKINGSLATE, THINKINGSLATE.slot, false),
        ("thinkingLow", THINKINGBLUE, THINKINGBLUE.slot, false),
        (
            "thinkingMedium",
            THINKINGPERIWINKLE,
            THINKINGPERIWINKLE.slot,
            false,
        ),
        ("thinkingHigh", THINKINGVIOLET, THINKINGVIOLET.slot, false),
        (
            "thinkingXhigh",
            THINKINGMAGENTA,
            THINKINGMAGENTA.slot,
            false,
        ),
        ("thinkingMax", THINKINGRED, THINKINGRED.slot, false),
    ];
    FAMILIES.iter().copied()
}

#[derive(Clone, Copy)]
pub(super) enum Level {
    Panel,
    Track,
    Thinking0,
    Thinking1,
    Thinking2,
    Thinking3,
    Thinking4,
    Thinking5,
    Thinking6,
    Subtle,
    Thumb,
    Readable,
    Emphasis,
    TextOnPanel,
    Text,
}

impl Level {
    pub(super) fn target(self, lighter: bool, lightness: f64) -> Option<f64> {
        let (coefficients, [min, max]) = match (self, lighter) {
            (Self::Panel, true) => (
                [0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276],
                [0.0, 0.979],
            ),
            (Self::Panel, false) => (
                [-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277],
                [0.348, 1.0],
            ),
            (Self::Track, true) => (
                [0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582],
                [0.0, 0.946],
            ),
            (Self::Track, false) => (
                [
                    -5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061,
                ],
                [0.368, 1.0],
            ),
            (Self::Thinking0, true) => (
                [0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108],
                [0.0, 0.873],
            ),
            (Self::Thinking0, false) => (
                [
                    -28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147,
                ],
                [0.51, 1.0],
            ),
            (Self::Thinking1, true) => (
                [0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706],
                [0.0, 0.858],
            ),
            (Self::Thinking1, false) => (
                [
                    -37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551,
                ],
                [0.535, 1.0],
            ),
            (Self::Thinking2, true) => (
                [0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532],
                [0.0, 0.842],
            ),
            (Self::Thinking2, false) => (
                [
                    -59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658,
                ],
                [0.556, 1.0],
            ),
            (Self::Thinking3, true) => (
                [0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606],
                [0.0, 0.827],
            ),
            (Self::Thinking3, false) => (
                [
                    -72.07122,
                    445.84082,
                    -1099.57352,
                    1353.88793,
                    -829.53392,
                    202.26164,
                ],
                [0.58, 1.0],
            ),
            (Self::Thinking4, true) => (
                [0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333],
                [0.0, 0.811],
            ),
            (Self::Thinking4, false) => (
                [
                    -110.14338,
                    674.21488,
                    -1645.75941,
                    2004.32367,
                    -1215.15899,
                    293.3183,
                ],
                [0.6, 1.0],
            ),
            (Self::Thinking5, true) => (
                [0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651],
                [0.0, 0.795],
            ),
            (Self::Thinking5, false) => (
                [
                    -175.47701,
                    1063.54495,
                    -2570.70594,
                    3098.80776,
                    -1860.15527,
                    444.76392,
                ],
                [0.62, 1.0],
            ),
            (Self::Thinking6, true) => (
                [0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439],
                [0.0, 0.779],
            ),
            (Self::Thinking6, false) => (
                [
                    -183.81712,
                    1094.70055,
                    -2602.68539,
                    3088.71276,
                    -1826.91131,
                    430.75931,
                ],
                [0.643, 1.0],
            ),
            (Self::Subtle, true) => (
                [0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324],
                [0.0, 0.848],
            ),
            (Self::Subtle, false) => (
                [
                    -232.85459,
                    1376.54473,
                    -3249.11801,
                    3827.91186,
                    -2248.29472,
                    526.55751,
                ],
                [0.657, 1.0],
            ),
            (Self::Thumb, true) => (
                [0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933],
                [0.0, 0.823],
            ),
            (Self::Thumb, false) => (
                [
                    -82.89897,
                    511.01355,
                    -1255.98095,
                    1540.76821,
                    -940.68087,
                    228.58523,
                ],
                [0.586, 1.0],
            ),
            (Self::Readable, true) => (
                [0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229],
                [0.0, 0.77],
            ),
            (Self::Readable, false) => (
                [
                    -1554.52576,
                    8733.56817,
                    -19604.93507,
                    21977.72696,
                    -12300.99599,
                    2749.81288,
                ],
                [0.751, 1.0],
            ),
            (Self::Emphasis, true) => (
                [0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846],
                [0.0, 0.712],
            ),
            (Self::Emphasis, false) => (
                [
                    -4948.31942,
                    26870.91986,
                    -58334.48399,
                    63280.17197,
                    -34298.01053,
                    7430.30146,
                ],
                [0.811, 1.0],
            ),
            (Self::TextOnPanel, true) => (
                [0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023],
                [0.0, 0.542],
            ),
            (Self::TextOnPanel, false) => (
                [
                    -8570.89457,
                    43954.60805,
                    -90084.00702,
                    92220.6791,
                    -47152.15802,
                    9632.27113,
                ],
                [0.867, 1.0],
            ),
            (Self::Text, true) => (
                [0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844],
                [0.0, 0.5],
            ),
            (Self::Text, false) => (
                [
                    -2004.67048,
                    6664.47299,
                    -6060.70202,
                    -1792.61209,
                    5133.82359,
                    -1939.85583,
                ],
                [0.894, 1.0],
            ),
        };
        if !(min..=max).contains(&lightness) {
            return None;
        }
        Some(
            coefficients
                .into_iter()
                .rev()
                .fold(0.0, |sum, coefficient| sum * lightness + coefficient),
        )
    }
}

pub(super) type Rule = (Level, &'static [&'static str], &'static [&'static str]);

/// (Contrast level, surfaces drawn on, tokens); surfaces occur before their dependent tokens.
pub(super) const RULES: &[Rule] = &[
    (
        Level::Panel,
        &["background"],
        &[
            "userMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
            "selectedBg",
            "searchMatchBg",
            "customMessageBg",
        ],
    ),
    (Level::Track, &["background"], &["scrollbarTrack"]),
    (Level::Text, &["background"], &["text"]),
    (Level::TextOnPanel, &["selectedBg"], &["text"]),
    (Level::TextOnPanel, &["userMessageBg"], &["userMessageText"]),
    (
        Level::TextOnPanel,
        &["toolPendingBg", "toolSuccessBg", "toolErrorBg"],
        &["toolTitle"],
    ),
    (
        Level::Readable,
        &[
            "background",
            "selectedBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        &["accent", "success", "error", "warning"],
    ),
    (
        Level::Readable,
        &[
            "background",
            "selectedBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        &["muted"],
    ),
    (
        Level::Subtle,
        &[
            "background",
            "selectedBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        &["dim"],
    ),
    (Level::Readable, &["background"], &["thinkingText"]),
    (
        Level::Readable,
        &[
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        &["customMessageText"],
    ),
    (
        Level::Readable,
        &[
            "background",
            "customMessageBg",
            "selectedBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        &["customMessageLabel"],
    ),
    (
        Level::Readable,
        &[
            "background",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        &[
            "toolOutput",
            "toolDiffAdded",
            "toolDiffRemoved",
            "toolDiffContext",
        ],
    ),
    (
        Level::Readable,
        &["background", "userMessageBg", "customMessageBg"],
        &[
            "mdHeading",
            "mdLink",
            "mdLinkUrl",
            "mdCode",
            "mdQuote",
            "mdCodeBlockBorder",
            "mdListBullet",
        ],
    ),
    (
        Level::Readable,
        &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        &[
            "mdCodeBlock",
            "syntaxComment",
            "syntaxKeyword",
            "syntaxFunction",
            "syntaxVariable",
            "syntaxString",
            "syntaxNumber",
            "syntaxType",
            "syntaxOperator",
            "syntaxPunctuation",
            "mdQuoteBorder",
            "mdHr",
        ],
    ),
    (Level::Readable, &["searchMatchBg"], &["searchMatchText"]),
    (
        Level::Readable,
        &["background"],
        &["bashMode", "border", "borderAccent"],
    ),
    (Level::Subtle, &["background"], &["borderMuted"]),
    (Level::Thumb, &["scrollbarTrack"], &["scrollbarThumb"]),
    (Level::Thinking0, &["background"], &["thinkingOff"]),
    (Level::Thinking1, &["background"], &["thinkingMinimal"]),
    (Level::Thinking2, &["background"], &["thinkingLow"]),
    (Level::Thinking3, &["background"], &["thinkingMedium"]),
    (Level::Thinking4, &["background"], &["thinkingHigh"]),
    (Level::Thinking5, &["background"], &["thinkingXhigh"]),
    (Level::Thinking6, &["background"], &["thinkingMax"]),
];
