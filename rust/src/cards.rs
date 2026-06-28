//! cards.rs — port of cards.py. Static card data loaded from krarkashima.txt, with the
//! curated TYPES / SUBTYPES / ENGINE overlays. NO STUBS: every registry line resolves to a
//! full `CardDef`.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

// A tiny FNV-1a hasher for the card registry's name->CardDef map. The registry is a hot
// read path (`reg.get(name)` runs on essentially every DFS node / rollout step), and the
// default SipHash is overkill for short ASCII card names. FNV is ~several× faster here and
// the map is never iterated in a result-affecting way (display order comes from the separate
// `order` Vec), so swapping the hasher is behavior-neutral.
#[derive(Default)]
pub struct FnvHasher(u64);
impl Hasher for FnvHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 { 0xcbf29ce484222325 } else { self.0 };
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        self.0 = h;
    }
}
type FnvBuild = BuildHasherDefault<FnvHasher>;
type FnvMap<K, V> = HashMap<K, V, FnvBuild>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CardType {
    Instant,
    Sorcery,
    Creature,
    Artifact,
    Enchantment,
    Land,
    Planeswalker,
}

/// A mana cost is a map of symbol -> count. Colors are "W"/"U"/"B"/"R"/"G"/"C", generic is
/// the key "generic", X is "X", wildcard (any-color, e.g. produced mana) is "*".
pub type ManaCost = HashMap<String, i64>;

/// `'{2}{U}{R}' -> {'generic':2,'U':1,'R':1}`. Handles {0}, hybrid/phyrexian (colored half),
/// X (under "X"). Lands ('Land'/'Basic Land') return {}. Mirror of cards.parse_mana_cost.
pub fn parse_mana_cost(s: &str) -> ManaCost {
    let s = s.trim();
    let mut cost: ManaCost = HashMap::new();
    if s == "Land" || s == "Basic Land" || s.is_empty() {
        return cost;
    }
    // find every {...} token
    let mut tok = String::new();
    let mut in_brace = false;
    for ch in s.chars() {
        match ch {
            '{' => {
                in_brace = true;
                tok.clear();
            }
            '}' => {
                in_brace = false;
                add_token(&mut cost, &tok);
            }
            _ if in_brace => tok.push(ch),
            _ => {}
        }
    }
    cost
}

fn add_token(cost: &mut ManaCost, tok: &str) {
    if let Ok(n) = tok.parse::<i64>() {
        *cost.entry("generic".to_string()).or_insert(0) += n;
    } else if matches!(tok, "W" | "U" | "B" | "R" | "G" | "C") {
        *cost.entry(tok.to_string()).or_insert(0) += 1;
    } else if tok == "X" {
        *cost.entry("X".to_string()).or_insert(0) += 1;
    } else if tok.contains('/') {
        if tok.split('/').any(|c| c == "P") {
            // Phyrexian (e.g. {U/P}, {R/P}): payable with 2 life. In this combo engine life is
            // abundant and never the binding constraint, so treat the pip as free (no cost added).
        } else {
            // hybrid, e.g. 2/U -> keep the colored half, else generic
            let half = tok.split('/').find(|c| matches!(*c, "W" | "U" | "B" | "R" | "G"));
            match half {
                Some(h) => *cost.entry(h.to_string()).or_insert(0) += 1,
                None => *cost.entry("generic".to_string()).or_insert(0) += 1,
            }
        }
    }
}

/// Extract Phyrexian pips from a raw cost string: `{U/P}` -> {U:1}, `{R/P}` -> {R:1}. A
/// generic-Phyrexian (no color, rare) maps to "generic". Non-Phyrexian tokens are ignored.
pub fn phyrexian_pips(s: &str) -> ManaCost {
    let mut out: ManaCost = HashMap::new();
    let mut tok = String::new();
    let mut in_brace = false;
    for ch in s.chars() {
        match ch {
            '{' => {
                in_brace = true;
                tok.clear();
            }
            '}' => {
                in_brace = false;
                if tok.contains('/') && tok.split('/').any(|c| c == "P") {
                    let color = tok.split('/').find(|c| matches!(*c, "W" | "U" | "B" | "R" | "G"));
                    let key = color.unwrap_or("generic").to_string();
                    *out.entry(key).or_insert(0) += 1;
                }
            }
            _ if in_brace => tok.push(ch),
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct CardDef {
    pub name: String,
    pub types: Vec<CardType>,
    pub cost: ManaCost,
    /// Phyrexian pips, by color (e.g. {U:1} for {U/P}). Excluded from `cost` (so they're free
    /// to "afford"); at cast time each pip is paid with 2 life, or its colored mana if life is low.
    pub phyrexian: ManaCost,
    pub mana_value: i64,
    pub rules_text: String,
    pub subtypes: Vec<String>,

    // ---- engine behaviour ----
    pub is_krark_body: bool,
    pub is_trigger_doubler: bool,
    pub clones_sakashima_safe: bool,

    pub draw_per_trigger: i64,
    pub treasure_per_trigger: i64,
    pub mana_per_trigger: ManaCost,
    pub damage_per_trigger: i64,
    pub treasure_per_flip_win: i64,
    pub trigger_cause: Option<String>,
    pub fires_on_copy: bool,
}

impl CardDef {
    pub fn blue_pips(&self) -> i64 {
        *self.cost.get("U").unwrap_or(&0)
    }
    pub fn is_instant_or_sorcery(&self) -> bool {
        self.types.contains(&CardType::Instant) || self.types.contains(&CardType::Sorcery)
    }
    pub fn is_creature(&self) -> bool {
        self.types.contains(&CardType::Creature)
    }
    pub fn is_artifact(&self) -> bool {
        self.types.contains(&CardType::Artifact)
    }
    pub fn is_land(&self) -> bool {
        self.types.contains(&CardType::Land)
    }
    pub fn is_permanent(&self) -> bool {
        self.types.iter().any(|t| {
            matches!(
                t,
                CardType::Creature
                    | CardType::Artifact
                    | CardType::Enchantment
                    | CardType::Land
                    | CardType::Planeswalker
            )
        })
    }
    pub fn is_shaman_or_wizard(&self) -> bool {
        self.subtypes.iter().any(|s| s == "Shaman" || s == "Wizard")
    }
    pub fn has_value_trigger(&self) -> bool {
        self.draw_per_trigger != 0
            || self.treasure_per_trigger != 0
            || !self.mana_per_trigger.is_empty()
            || self.damage_per_trigger != 0
            || self.treasure_per_flip_win != 0
    }
}

// --------------------------------------------------------------------------- //
// Overlays (the hand-verified layer) — mirror of cards.py
// --------------------------------------------------------------------------- //

const CREATURES: &[&str] = &[
    "Krark, the Thumbless", "Sakashima of a Thousand Faces", "Archmage Emeritus",
    "Baral, Chief of Compliance", "Birgi, God of Storytelling", "Dualcaster Mage",
    "Gale, Waterdeep Prodigy", "Harmonic Prodigy", "Imperial Recruiter",
    "Okaun, Eye of Chaos", "Ragavan, Nimble Pilferer", "Snapcaster Mage",
    "Spellseeker", "Storm-Kiln Artist", "Tavern Scoundrel", "Urabrask",
    "Veyran, Voice of Duality", "Vivi Ornitier", "Zndrsplt, Eye of Wisdom",
    "Glasspool Mimic", "Phantasmal Image", "Subtlety", "Thassa's Oracle",
    "Valley Floodcaller", "Phyrexian Metamorph", "Mockingbird", "Roaming Throne",
    "Electro, Assaulting Battery",
];
const SORCERIES: &[&str] = &[
    "Quasiduplicate", "Gamble", "Jeska's Will", "Ponder", "Grapeshot",
    "Twinflame", "Strike It Rich", "Gitaxian Probe", "Rite of Flame", "Heat Shimmer",
    "Serum Visions", "Preordain", "Overmaster", "Heroes' Hangout", "Step Through",
    "Renegade Tactics",
];
const INSTANTS: &[&str] = &[
    "Brainstorm", "Cyclonic Rift", "Brain Freeze", "Frantic Search", "Snap",
    "Gut Shot", "Desperate Ritual", "Pyretic Ritual", "Deflecting Swat",
    "Fierce Guardianship", "Flusterstorm", "Force of Will", "Pact of Negation",
    "An Offer You Can't Refuse", "Borne Upon a Wind", "Mogg Salvage", "Peek",
    "Opt", "Consider", "Expedite", "Might of the Meek", "Brightstone Ritual",
    "Crimson Wisps", "Accelerate",
    "Mystical Tutor",
];
const ENCHANTMENTS: &[&str] = &["Underworld Breach", "Mystic Remora", "Rhystic Study"];
const ARTIFACTS: &[&str] = &[
    "Defense Grid", "Arcane Signet", "Chrome Mox", "Krark's Thumb",
    "Lion's Eye Diamond", "Lotus Petal", "Mox Diamond", "Sol Ring",
    "Springleaf Drum", "The One Ring", "Mana Vault", "Mox Amber", "Relic of Legends",
    "Simian Spirit Guide", "Talisman of Creativity", "Grim Monolith",
    "Roaming Throne", // Artifact Creature — Spirit; also in CREATURES (dual-typed via additive classify)
];

fn subtypes_for(name: &str) -> Vec<String> {
    let v: &[&str] = match name {
        "Krark, the Thumbless" => &["Goblin", "Wizard"],
        "Archmage Emeritus" => &["Human", "Wizard"],
        "Storm-Kiln Artist" => &["Dwarf", "Shaman"],
        "Veyran, Voice of Duality" => &["Human", "Wizard"],
        "Harmonic Prodigy" => &["Human", "Shaman"],
        "Birgi, God of Storytelling" => &["God"],
        "Electro, Assaulting Battery" => &["Human", "Villain"],
        "Urabrask" => &["Phyrexian", "Praetor"],
        "Vivi Ornitier" => &["Wizard"],
        _ => &[],
    };
    v.iter().map(|s| s.to_string()).collect()
}

/// Apply the ENGINE overlay for `name` onto `cd`. Mirror of cards.ENGINE.
fn apply_engine(name: &str, cd: &mut CardDef) {
    let mut mana = |c: &str| {
        let mut m: ManaCost = HashMap::new();
        m.insert(c.to_string(), 1);
        m
    };
    match name {
        "Krark, the Thumbless" => cd.is_krark_body = true,
        "Sakashima of a Thousand Faces" => cd.clones_sakashima_safe = true,
        "Veyran, Voice of Duality" => cd.is_trigger_doubler = true,
        "Harmonic Prodigy" => cd.is_trigger_doubler = true,
        "Roaming Throne" => cd.is_trigger_doubler = true, // choose Wizard: doubles Krark flips + Wizard magecraft
        "Archmage Emeritus" => {
            cd.draw_per_trigger = 1;
            cd.trigger_cause = Some("is_cast_or_copy".into());
            cd.fires_on_copy = true;
        }
        "Storm-Kiln Artist" => {
            cd.treasure_per_trigger = 1;
            cd.trigger_cause = Some("is_cast_or_copy".into());
            cd.fires_on_copy = true;
        }
        "Birgi, God of Storytelling" => {
            cd.mana_per_trigger = mana("R");
            cd.trigger_cause = Some("spell_cast".into());
        }
        // Electro adds {R} only on instant/sorcery casts (Birgi triggers on any spell), so use the
        // I/S "is_cast" cause; the persistent-mana clause matches Birgi's within-turn retention.
        "Electro, Assaulting Battery" => {
            cd.mana_per_trigger = mana("R");
            cd.trigger_cause = Some("is_cast".into());
        }
        "Urabrask" => {
            cd.mana_per_trigger = mana("R");
            cd.damage_per_trigger = 1;
            cd.trigger_cause = Some("is_cast".into());
        }
        "Tavern Scoundrel" => {
            cd.treasure_per_flip_win = 2;
            cd.trigger_cause = Some("coin_flip_win".into());
        }
        "Vivi Ornitier" => {
            // 3 damage (one per opponent in a 4-player pod) on each I/S CAST -- not on Krark copies.
            // The per-cast {*} mana infusion is UNCONFIRMED for this card, so it is NOT modeled.
            cd.damage_per_trigger = 3;
            cd.trigger_cause = Some("is_cast".into());
        }
        "Zndrsplt, Eye of Wisdom" => {
            cd.draw_per_trigger = 1;
            cd.trigger_cause = Some("coin_flip_win".into());
        }
        _ => {}
    }
}

/// Cards on the VERIFY list (interaction not independently confirmed). cards.VERIFY keys.
pub fn is_verify(name: &str) -> bool {
    name == "Tavern Scoundrel"
}

fn classify(name: &str, raw_cost: &str, rules: &str) -> Vec<CardType> {
    if raw_cost == "Land" || raw_cost == "Basic Land" {
        return vec![CardType::Land];
    }
    // Additive: a card can carry multiple types (e.g. an artifact creature like Roaming Throne),
    // so check every overlay list rather than early-returning on the first match.
    let mut types = Vec::new();
    if CREATURES.contains(&name) { types.push(CardType::Creature); }
    if SORCERIES.contains(&name) { types.push(CardType::Sorcery); }
    if INSTANTS.contains(&name) { types.push(CardType::Instant); }
    if ENCHANTMENTS.contains(&name) { types.push(CardType::Enchantment); }
    if ARTIFACTS.contains(&name) { types.push(CardType::Artifact); }
    if !types.is_empty() {
        return types;
    }
    // last resort: read a leading type line from the rules text.
    for (word, ty) in [
        ("Creature", CardType::Creature),
        ("Artifact", CardType::Artifact),
        ("Enchantment", CardType::Enchantment),
        ("Land", CardType::Land),
        ("Sorcery", CardType::Sorcery),
        ("Instant", CardType::Instant),
        ("Planeswalker", CardType::Planeswalker),
    ] {
        if rules.contains(word) {
            return vec![ty];
        }
    }
    panic!("Cannot classify card type for {name:?}; add it to an overlay set.");
}

// --------------------------------------------------------------------------- //
// Registry
// --------------------------------------------------------------------------- //

pub struct Registry {
    map: FnvMap<String, CardDef>,
    order: Vec<String>, // insertion order, mirrors Python dict ordering for build_deck
}

impl Registry {
    pub fn load(text: &str) -> Registry {
        let mut map: FnvMap<String, CardDef> = FnvMap::default();
        let mut order = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if i == 0 {
                assert!(line.starts_with("name|"), "unexpected header: {line}");
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            assert!(parts.len() == 4, "bad line: {line}");
            let name = parts[0].trim().to_string();
            let raw_cost = parts[1].trim();
            let mv = parts[2].trim();
            let rules = parts[3].trim();
            let types = classify(&name, raw_cost, rules);
            let mut cd = CardDef {
                name: name.clone(),
                types,
                cost: parse_mana_cost(raw_cost),
                phyrexian: phyrexian_pips(raw_cost),
                mana_value: mv.parse::<i64>().unwrap_or(0),
                rules_text: rules.to_string(),
                subtypes: subtypes_for(&name),
                ..Default::default()
            };
            apply_engine(&name, &mut cd);
            if !map.contains_key(&name) {
                order.push(name.clone());
            }
            map.insert(name, cd);
        }
        Registry { map, order }
    }

    pub fn get(&self, name: &str) -> &CardDef {
        self.map
            .get(name)
            .unwrap_or_else(|| panic!("Unknown card {name:?}. Is it in krarkashima.txt?"))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn ordered_names(&self) -> &[String] {
        &self.order
    }
}

// --------------------------------------------------------------------------- //
// Targeting legality in the solitaire model — mirror of cards.py constants
// --------------------------------------------------------------------------- //

pub const NO_SOLITAIRE_TARGET: &[&str] = &[
    // Force of Will pitches a card per cast (can't loop); Subtlety is evoke/pitch interaction.
    // The other free/cheap counters are now LOOPABLE magecraft fuel (loops::MAGECRAFT_FUEL):
    // cast for value off a per-cast engine, bouncing off a second spell on the stack (assumes
    // the opponent always has a legal target — Cyclonic Rift bounces THEIR permanent).
    "Force of Will",
    "Subtlety",
];
pub const FREE_COUNTERS: &[&str] = &["Deflecting Swat"];
pub const NEEDS_OWN_CREATURE: &[&str] = &["Twinflame", "Heat Shimmer", "Quasiduplicate", "Snap"];

pub fn castable_in_solitaire(name: &str, has_own_creature: bool) -> bool {
    if NO_SOLITAIRE_TARGET.contains(&name) {
        return false;
    }
    if NEEDS_OWN_CREATURE.contains(&name) {
        return has_own_creature;
    }
    true
}
