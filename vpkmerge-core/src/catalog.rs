//! Build the Foundry asset catalog from a Deadlock install.
//!
//! The marquee piece is the **voice-line search index**: one searchable row per
//! VO sound event ([`VoiceLine`]), carrying the event name, hero, clip path(s), a
//! human-readable label, and the engine duration. A UI searches it (by the
//! descriptive label, e.g. "ally atlas killed in lane") and forges a swap for the
//! chosen event.
//!
//! Note on subtitles: the game also ships a compiled caption database
//! (`resource/localization/citadel_generated_vo/citadel_generated_vo_<lang>.dat`,
//! VCCD v2, keyed by `crc32(token)`). [`CaptionDb`] reads it, but empirically the
//! per-hero VO events all map to *empty* English captions (Deadlock does not
//! subtitle hero combat barks); the authored caption text lives under a separate
//! token namespace not tied to swappable sound events. So caption text is only a
//! best-effort enrichment on the index, almost always `None` for hero VO. The
//! searchable text is the descriptive event name itself. Findings pinned down
//! against the live pak; see `grimoire/docs/foundry-tab-design.md`.
//!
//! Pure Rust, no extra dependencies (the caption hash is the standard
//! CRC-32/ISO-HDLC, the same one `zlib`/`gzip` use).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use morphic::kv3::Value;

use crate::soundevents::SoundEvents;

/// Entry path of the English compiled VO captions inside `citadel/pak01`. Other
/// languages live at the same relative path inside their `citadel_<lang>` pak.
pub const ENGLISH_CAPTIONS_ENTRY: &str =
    "resource/localization/citadel_generated_vo/citadel_generated_vo_english.dat";

/// Root of every soundevents tree in the pak.
const SOUNDEVENTS_PREFIX: &str = "soundevents/";

/// The per-hero / announcer VO tree.
const VO_TREE_PREFIX: &str = "soundevents/vo/";
/// Per-hero VO files are named `generated_vo_hero_<code>.vsndevts_c`.
const HERO_VO_STEM: &str = "generated_vo_hero_";

/// The per-hero gameplay-sound tree: one `soundevents/hero/<code>.vsndevts_c`
/// per hero, holding weapon / ability / movement / melee events (the non-VO
/// counterpart to the VO tree).
const HERO_TREE_PREFIX: &str = "soundevents/hero/";

/// Root of every soundevents tree in the pak.
const SOUNDEVENTS_PREFIX_ALL: &str = "soundevents/";

/// How far to walk a `base` chain before giving up. Real chains are 1-2 links
/// (`Abrams.Wpn.Impact` -> `Base.Bullet.Impact.Shotgun` -> `Base.Bullet.Impact`);
/// the cap plus the visited set is belt-and-braces against a malformed pak
/// pointing a template at itself.
const MAX_BASE_DEPTH: usize = 8;

/// Every event in the pak that can serve as an inheritance target: event name ->
/// (its own clips, its `base` ref). Built once and shared by the hero and global
/// indexes.
///
/// This has to span the WHOLE `soundevents/` tree, not one file: templates are
/// referenced across files with no locality at all. `Abrams.Wpn.Whizby` lives in
/// `soundevents/hero/abrams.vsndevts_c` and inherits `Base.Bullet.Whizby.Shotgun`
/// from `soundevents/base/weapons_shared.vsndevts_c`, which in turn inherits
/// `Base.Bullet.Impact` from `soundevents/base/citadel_base_events.vsndevts_c`.
type BaseTable = HashMap<String, (Vec<String>, Option<String>)>;

fn build_base_table(vpk: &valve_pak::VPK) -> BaseTable {
    let mut entries: Vec<String> = vpk
        .file_paths()
        .filter(|p| p.starts_with(SOUNDEVENTS_PREFIX_ALL) && p.ends_with(".vsndevts_c"))
        .cloned()
        .collect();
    entries.sort();

    let mut table: BaseTable = HashMap::new();
    for entry in &entries {
        let Ok(mut file) = vpk.get_file(entry) else {
            continue;
        };
        let Ok(bytes) = file.read_all() else {
            continue;
        };
        // A file that fails to decode must not sink the table; the events it
        // would have contributed simply stay unresolvable.
        let Ok(se) = SoundEvents::from_bytes(bytes) else {
            continue;
        };
        let Value::Object(pairs) = &se.root else {
            continue;
        };
        for (name, event) in pairs {
            if name == "base" {
                continue;
            }
            table.insert(
                name.clone(),
                (
                    event_own_clips(event),
                    event.get("base").and_then(Value::as_str).map(str::to_owned),
                ),
            );
        }
    }
    table
}

/// An event's own `vsnd_files`, normalizing the single-string spelling to a
/// one-element list. Empty when the event declares none.
fn event_own_clips(event: &Value) -> Vec<String> {
    match event.get("vsnd_files") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Resolve the clips an event actually plays, following `base` when it declares
/// none of its own. Returns the clips plus the template they came from, or
/// `None` for that second field when the event owns its clips outright.
///
/// **The inherited case is a shared pool, and callers must treat it as one.**
/// `Abrams.Wpn.Whizby` has no clips; it plays
/// `Base.Bullet.Whizby.Shotgun`'s 20. Those same clips back every other
/// shotgun hero's whizby, so overriding them changes the sound for all of
/// them, not just Abrams. That is why the source template is returned rather
/// than silently folded in: a UI needs to be able to say so before a user
/// swaps.
fn resolve_clips(
    own: Vec<String>,
    base: Option<&str>,
    table: &BaseTable,
) -> (Vec<String>, Option<String>) {
    if !own.is_empty() {
        return (own, None);
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut cursor = base;
    for _ in 0..MAX_BASE_DEPTH {
        let Some(name) = cursor else { break };
        // Cycle guard: a template chain that revisits a name would loop forever.
        if seen.contains(&name) {
            break;
        }
        seen.push(name);
        let Some((clips, next)) = table.get(name) else {
            break; // dangling template reference
        };
        if !clips.is_empty() {
            return (clips.clone(), Some(name.to_owned()));
        }
        cursor = next.as_deref();
    }
    (Vec::new(), None)
}

/// One VO sound event, ready to search and swap.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoiceLine {
    /// Soundevent name, e.g. `bebop_self_ultimate_cast_01_hero_3d`. Usable
    /// verbatim as the swap target in the soundevents layer.
    pub event: String,
    /// Hero codename (`bebop`, `astro`, ...) from the event's `context_name`,
    /// falling back to the source filename. `None` for announcer / non-hero VO.
    pub hero: Option<String>,
    /// Human-readable label derived from the event name, e.g.
    /// `"ally atlas killed in lane"`. This is the searchable text.
    pub label: String,
    /// Clip path(s) the event plays (more than one == a randomizer pool).
    pub vsnd: Vec<String>,
    /// Engine playback duration in seconds, if the event records one.
    pub duration: Option<f64>,
    /// Authored English subtitle, if the caption database resolves this event.
    /// Almost always `None` for hero VO; see the module note.
    pub caption: Option<String>,
}

/// A decoded VCCD (Valve Compiled Caption Database).
///
/// Maps `crc32(token)` to caption text. For generated VO the token is the
/// soundevent name, so [`Self::for_event`] resolves a line's subtitle directly.
pub struct CaptionDb {
    by_hash: HashMap<u32, String>,
}

impl CaptionDb {
    /// Parse a `.dat` caption database (VCCD v2).
    ///
    /// Header (24 bytes, little-endian): `magic 'VCCD' | version | numBlocks |
    /// blockSize | dirEntries | dataOffset`, then `dirEntries` 12-byte directory
    /// records (`u32 hash | i32 blockNum | u16 offset | u16 length`), then
    /// UTF-16LE NUL-terminated strings packed into `blockSize` blocks beginning
    /// at `dataOffset`.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 24 || &bytes[0..4] != b"VCCD" {
            bail!("not a VCCD caption database (bad magic)");
        }
        let rd = |off: usize| u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        let version = rd(4);
        if version != 1 && version != 2 {
            bail!("unsupported VCCD version {version}");
        }
        let block_size = rd(12) as usize;
        let dir_entries = rd(16) as usize;
        let data_offset = rd(20) as usize;

        let dir_start = 24usize;
        let dir_bytes = dir_entries
            .checked_mul(12)
            .and_then(|n| n.checked_add(dir_start))
            .context("caption directory size overflow")?;
        if dir_bytes > bytes.len() {
            bail!("caption directory ({dir_entries} entries) runs past end of file");
        }

        let mut by_hash: HashMap<u32, String> = HashMap::with_capacity(dir_entries);
        for i in 0..dir_entries {
            let base = dir_start + i * 12;
            let hash = u32::from_le_bytes(bytes[base..base + 4].try_into().unwrap());
            let block = i32::from_le_bytes(bytes[base + 4..base + 8].try_into().unwrap());
            let offset =
                u16::from_le_bytes(bytes[base + 8..base + 10].try_into().unwrap()) as usize;
            let length = usize::from(u16::from_le_bytes(
                bytes[base + 10..base + 12].try_into().unwrap(),
            ));
            let Ok(block) = usize::try_from(block) else {
                continue;
            };
            if length == 0 {
                // Empty caption (common for routine combat callouts): skip it,
                // but never let a later empty dup clobber a real string.
                by_hash.entry(hash).or_default();
                continue;
            }
            let start = data_offset + block * block_size + offset;
            let Some(raw) = bytes.get(start..start + length) else {
                continue;
            };
            let text = decode_utf16le(raw);
            if text.is_empty() {
                by_hash.entry(hash).or_default();
            } else {
                // Prefer a non-empty string on a hash collision.
                by_hash.insert(hash, text);
            }
        }
        Ok(Self { by_hash })
    }

    /// Caption text for a raw token hash, if present and non-empty.
    #[must_use]
    pub fn by_hash(&self, hash: u32) -> Option<&str> {
        self.by_hash
            .get(&hash)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Caption text for a token string (hashed with [`caption_hash`]).
    #[must_use]
    pub fn get(&self, token: &str) -> Option<&str> {
        self.by_hash(caption_hash(token))
    }

    /// Caption text for a soundevent name (the generated-VO token is the event
    /// name verbatim).
    #[must_use]
    pub fn for_event(&self, event: &str) -> Option<&str> {
        self.get(event)
    }

    /// Total directory entries retained (including empty captions).
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Number of distinct hashes carrying non-empty caption text.
    #[must_use]
    pub fn non_empty_count(&self) -> usize {
        self.by_hash.values().filter(|s| !s.is_empty()).count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

/// Build the voice-line search index for one VPK whose caption database it also
/// carries (the base `citadel/pak01` for English). Convenience over
/// [`build_voiceline_index_with_captions`].
pub fn build_voiceline_index(vpk_path: impl AsRef<Path>) -> Result<Vec<VoiceLine>> {
    let vpk_path = vpk_path.as_ref();
    let caption_bytes = crate::read_vpk_entry(vpk_path, ENGLISH_CAPTIONS_ENTRY)
        .with_context(|| format!("reading {ENGLISH_CAPTIONS_ENTRY}"))?;
    let captions = CaptionDb::parse(&caption_bytes)?;
    build_voiceline_index_with_captions(vpk_path, &captions)
}

/// Build the voice-line index from the VO soundevents tree in `vpk_path`,
/// enriching each row with caption text from `captions` where it resolves. Use
/// this overload when the captions live in a different pak (a non-English
/// `citadel_<lang>` pak).
///
/// Every event in `soundevents/vo/*.vsndevts_c` becomes a [`VoiceLine`]; the
/// `base` inheritance template is not an event and is skipped. Lines are sorted
/// by hero then event for a stable index.
pub fn build_voiceline_index_with_captions(
    vpk_path: impl AsRef<Path>,
    captions: &CaptionDb,
) -> Result<Vec<VoiceLine>> {
    let vpk_path = vpk_path.as_ref();
    let vpk =
        valve_pak::open(vpk_path).with_context(|| format!("opening {}", vpk_path.display()))?;

    let mut vo_entries: Vec<String> = vpk
        .file_paths()
        .filter(|p| p.starts_with(VO_TREE_PREFIX) && p.ends_with(".vsndevts_c"))
        .cloned()
        .collect();
    vo_entries.sort();

    let mut lines = Vec::new();
    for entry in &vo_entries {
        let file_hero = hero_from_vo_entry(entry);
        let mut file = vpk
            .get_file(entry)
            .with_context(|| format!("locating {entry}"))?;
        let bytes = file
            .read_all()
            .with_context(|| format!("reading {entry}"))?;
        // A VO file that fails to decode should not sink the whole index.
        let Ok(se) = SoundEvents::from_bytes(bytes) else {
            continue;
        };
        let Value::Object(pairs) = &se.root else {
            continue;
        };
        for (name, event) in pairs {
            // The `base` key is a shared template, not a playable event.
            if name == "base" {
                continue;
            }
            let hero = event
                .get("context_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| file_hero.clone());
            let vsnd = match event.get("vsnd_files") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                Some(Value::String(s)) => vec![s.clone()],
                _ => Vec::new(),
            };
            lines.push(VoiceLine {
                label: pretty_label(name, hero.as_deref()),
                caption: captions.for_event(name).map(str::to_owned),
                duration: event.get("vsnd_duration").and_then(Value::as_f64),
                hero,
                vsnd,
                event: name.clone(),
            });
        }
    }

    lines.sort_by(|a, b| a.hero.cmp(&b.hero).then_with(|| a.event.cmp(&b.event)));
    Ok(lines)
}

/// Hero codename from a VO entry path, or `None` for announcer / non-hero VO.
/// `soundevents/vo/generated_vo_hero_bebop.vsndevts_c` -> `Some("bebop")`.
fn hero_from_vo_entry(entry: &str) -> Option<String> {
    let stem = entry.rsplit('/').next()?.strip_suffix(".vsndevts_c")?;
    stem.strip_prefix(HERO_VO_STEM).map(str::to_owned)
}

/// Turn a soundevent name into searchable prose: drop the speaker (hero) prefix,
/// the `_hero_3d` / `_2d` channel suffix and a trailing take number, then spell
/// the underscores as spaces. `bebop_ally_atlas_killed_in_lane_01_hero_3d` with
/// hero `bebop` -> `"ally atlas killed in lane"`.
fn pretty_label(event: &str, hero: Option<&str>) -> String {
    let mut s = event;
    if let Some(h) = hero {
        if let Some(rest) = s.strip_prefix(h) {
            s = rest.trim_start_matches('_');
        }
    }
    for suffix in ["_hero_3d", "_hero_2d", "_3d", "_2d"] {
        if let Some(rest) = s.strip_suffix(suffix) {
            s = rest;
            break;
        }
    }
    // Trailing take/variant segments (`_01`, `_alt`, and combinations like
    // `_01_alt_01`), peeled one segment at a time.
    while let Some((head, tail)) = s.rsplit_once('_') {
        if tail == "alt" || (!tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())) {
            s = head;
        } else {
            break;
        }
    }
    s.replace('_', " ").trim().to_owned()
}

/// Decode a UTF-16LE byte slice, dropping the trailing NUL terminator(s).
fn decode_utf16le(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// CRC-32/ISO-HDLC (the `zlib`/`gzip` CRC): reflected poly `0xEDB88320`, init
/// and xorout `0xFFFFFFFF`. This is the hash VCCD uses to key caption tokens.
#[must_use]
pub fn caption_hash(token: &str) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in token.as_bytes() {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Which broad family a hero gameplay sound belongs to. Derived from the event
/// name's grouping segment (`Haze.Wpn.Fire.Main` -> `Weapon`,
/// `Haze.Finesse.Dagger.Cast` -> `Ability`); see [`build_hero_sound_index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeroSoundCategory {
    /// Primary-weapon sounds: fire, reload, zoom, whizby, impact, foley.
    Weapon,
    /// A hero ability (named, e.g. `Finesse`, or slot-tagged `A1`..`A4`).
    Ability,
    /// Locomotion: footsteps, jump/land, mantle, ladder steps.
    Movement,
    /// Melee swing.
    Melee,
    /// Anything that doesn't fit the above (e.g. the progression-page stingers).
    Other,
}

/// One playable hero gameplay sound event (weapon / ability / movement / melee),
/// the non-VO counterpart to [`VoiceLine`]. Read from `soundevents/hero/`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HeroSound {
    /// Soundevent name, e.g. `Haze.Finesse.Dagger.Cast`. Usable verbatim as the
    /// swap target in the soundevents layer.
    pub event: String,
    /// Hero "sound" codename from the source filename stem (`abrams`, `gigawatt`,
    /// `vampirebat`). This is the sound-path namespace, which diverges from the
    /// roster/script codename for some heroes (Abrams' roster codename is `atlas`).
    pub hero: String,
    /// Which family the event belongs to.
    pub category: HeroSoundCategory,
    /// The ability's display name when `category` is `Ability` (e.g. `Finesse`,
    /// `Siphon Life`), else `None`. The grouping key for an ability picker.
    pub ability: Option<String>,
    /// Ability slot 1..4 when the event carries an `A1`..`A4` token (Abrams), else
    /// `None`. Most heroes name abilities instead of slot-tagging them.
    pub slot: Option<u8>,
    /// Human-readable label derived from the event name, e.g. `"Fire Main"`,
    /// `"Dagger Cast"`. The searchable text.
    pub label: String,
    /// Clip path(s) the event plays (more than one == a randomizer pool).
    pub vsnd: Vec<String>,
    /// Engine playback duration in seconds, if the event records one.
    pub duration: Option<f64>,
    /// The template these clips were inherited from, when the event declares no
    /// `vsnd_files` of its own (`Abrams.Wpn.Whizby` ->
    /// `Base.Bullet.Whizby.Shotgun`). `None` means the event owns its clips.
    ///
    /// **Treat a `Some` here as a shared pool.** The same clips back every other
    /// event inheriting that template, so overriding them changes all of those
    /// sounds, not just this one. A UI should say so before offering the swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_from: Option<String>,
}

/// Build the hero gameplay-sound index from the `soundevents/hero/` tree in
/// `vpk_path`: one [`HeroSound`] per event across every `soundevents/hero/<code>
/// .vsndevts_c` (the shared `_shared` template file and `base` inheritance keys
/// are skipped). Rows are sorted by hero then event for a stable index.
///
/// This is the gameplay-sound counterpart to [`build_voiceline_index`]; together
/// they cover a hero's full audible surface (abilities + gun here, voice barks
/// there).
pub fn build_hero_sound_index(vpk_path: impl AsRef<Path>) -> Result<Vec<HeroSound>> {
    let vpk_path = vpk_path.as_ref();
    let vpk =
        valve_pak::open(vpk_path).with_context(|| format!("opening {}", vpk_path.display()))?;

    // Templates are referenced across files, so the inheritance table spans the
    // whole soundevents tree, not just the hero one. Built once per index.
    let base_table = build_base_table(&vpk);

    let mut entries: Vec<String> = vpk
        .file_paths()
        .filter(|p| p.starts_with(HERO_TREE_PREFIX) && p.ends_with(".vsndevts_c"))
        .cloned()
        .collect();
    entries.sort();

    let mut sounds = Vec::new();
    for entry in &entries {
        let Some(hero) = hero_from_hero_entry(entry) else {
            continue; // `_shared` and any other non-hero file
        };
        let mut file = vpk
            .get_file(entry)
            .with_context(|| format!("locating {entry}"))?;
        let bytes = file
            .read_all()
            .with_context(|| format!("reading {entry}"))?;
        // A file that fails to decode should not sink the whole index.
        let Ok(se) = SoundEvents::from_bytes(bytes) else {
            continue;
        };
        let Value::Object(pairs) = &se.root else {
            continue;
        };
        for (name, event) in pairs {
            // The `base` key is a shared template, not a playable event.
            if name == "base" {
                continue;
            }
            // An event with no `vsnd_files` of its own usually still plays
            // something: it inherits a template's pool. Resolving that is the
            // difference between indexing `Abrams.Wpn.Whizby` and dropping it.
            let (vsnd, inherited_from) = resolve_clips(
                event_own_clips(event),
                event.get("base").and_then(Value::as_str),
                &base_table,
            );
            // Still nothing after resolution: a pure parameter row, not playable.
            if vsnd.is_empty() {
                continue;
            }
            let mut classified = classify_hero_event(name);
            // Most heroes name abilities instead of slot-tagging the event, but
            // the clip path usually still carries an `aN` folder
            // (`sounds/abilities/<code>/a2_batblink/...`). Use it to recover the
            // slot so an ability picker can order 1..4 like in-game.
            if classified.slot.is_none() && classified.category == HeroSoundCategory::Ability {
                classified.slot = slot_from_vsnd(&vsnd);
            }
            sounds.push(HeroSound {
                category: classified.category,
                ability: classified.ability,
                slot: classified.slot,
                label: classified.label,
                duration: event.get("vsnd_duration").and_then(Value::as_f64),
                hero: hero.clone(),
                vsnd,
                inherited_from,
                event: name.clone(),
            });
        }
    }

    sounds.sort_by(|a, b| a.hero.cmp(&b.hero).then_with(|| a.event.cmp(&b.event)));
    Ok(sounds)
}

/// Hero sound-codename from a hero-tree entry path, or `None` for the shared
/// template. `soundevents/hero/abrams.vsndevts_c` -> `Some("abrams")`;
/// `soundevents/hero/_shared.vsndevts_c` -> `None`.
fn hero_from_hero_entry(entry: &str) -> Option<String> {
    let stem = entry.rsplit('/').next()?.strip_suffix(".vsndevts_c")?;
    if stem.starts_with('_') {
        return None;
    }
    Some(stem.to_owned())
}

/// Which broad family a non-hero (global) soundevent belongs to. Unlike hero
/// events, global event *names* follow no single convention across files
/// (`UI.Click`, `Powerup.Pickup`, `Trooper.Death`), so the category comes from
/// the event's **source file**, which is how the game itself groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GlobalSoundCategory {
    /// Menu / HUD / shop clicks, the chat wheel: `soundevents/ui`, `base/ui`.
    Ui,
    /// Score and menu music: `soundevents/music*`, `base/music`.
    Music,
    /// World ambience beds: the `soundevents/ambience/` tree, ambient emitters.
    Ambience,
    /// Non-player combatants: the `soundevents/npc/` tree (troopers, guardians,
    /// walkers, patron, neutral camps).
    Npc,
    /// Shop-item sounds: the `soundevents/mods/` tree (weapon / armor / tech).
    Item,
    /// Match gameplay: damage, status effects, powerups, ziplines, the local
    /// player, breakables, world objects, shared weapon layers.
    Gameplay,
    /// Non-hero-tree voice: announcer-adjacent and generated pick / death VO
    /// that lives outside `soundevents/vo/`.
    Voice,
    /// Anything else, including dev/tools files that still carry playable clips.
    Other,
}

/// One playable non-hero soundevent: UI, music, ambience, NPC, item, gameplay.
/// The counterpart to [`HeroSound`] for everything outside `soundevents/hero/`
/// and `soundevents/vo/`, which have their own indexes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GlobalSound {
    /// Soundevent name, e.g. `UI.Click`. Usable verbatim as `soundswap --event`.
    pub event: String,
    /// Entry path of the soundevents file the event was read from, e.g.
    /// `soundevents/ui.vsndevts_c`. This is what `soundswap --soundevents`
    /// wants: the global path has no hero to infer the file from.
    pub soundevents: String,
    /// Short source name derived from `soundevents`, e.g. `ui`, `npc/troopers`.
    /// The grouping key for a browse UI.
    pub source: String,
    /// Which family the event belongs to (from `source`, see the enum note).
    pub category: GlobalSoundCategory,
    /// Human-readable label derived from the event name, e.g. `"Ui Click"`.
    /// The searchable text.
    pub label: String,
    /// Clip path(s) the event plays (more than one == a randomizer pool).
    pub vsnd: Vec<String>,
    /// Engine playback duration in seconds, if the event records one.
    pub duration: Option<f64>,
    /// Template these clips came from when the event declares none of its own.
    /// A `Some` here means a SHARED pool: see [`resolve_clips`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_from: Option<String>,
}

/// Build the global (non-hero) sound index from `vpk_path`: one [`GlobalSound`]
/// per playable event across every `soundevents/**.vsndevts_c` that is *not* in
/// the hero (`soundevents/hero/`) or VO (`soundevents/vo/`) trees, which
/// [`build_hero_sound_index`] and [`build_voiceline_index`] already cover.
/// Together the three indexes span Deadlock's whole swappable audio surface.
///
/// Rows are sorted by source then event for a stable index. Events with no clips
/// (pure parameter/template rows, and the `base` inheritance key) are dropped, as
/// are files that fail to decode: one bad file must not sink the index.
pub fn build_global_sound_index(vpk_path: impl AsRef<Path>) -> Result<Vec<GlobalSound>> {
    let vpk_path = vpk_path.as_ref();
    let vpk =
        valve_pak::open(vpk_path).with_context(|| format!("opening {}", vpk_path.display()))?;

    // Same pak-wide inheritance table the hero index uses: global events defer
    // to templates across files exactly like hero ones do.
    let base_table = build_base_table(&vpk);

    let mut entries: Vec<String> = vpk
        .file_paths()
        .filter(|p| {
            p.starts_with(SOUNDEVENTS_PREFIX)
                && p.ends_with(".vsndevts_c")
                && !p.starts_with(HERO_TREE_PREFIX)
                && !p.starts_with(VO_TREE_PREFIX)
        })
        .cloned()
        .collect();
    entries.sort();

    let mut sounds = Vec::new();
    for entry in &entries {
        let source = global_source_name(entry);
        let category = classify_global_source(&source);
        let mut file = vpk
            .get_file(entry)
            .with_context(|| format!("locating {entry}"))?;
        let bytes = file
            .read_all()
            .with_context(|| format!("reading {entry}"))?;
        let Ok(se) = SoundEvents::from_bytes(bytes) else {
            continue;
        };
        let Value::Object(pairs) = &se.root else {
            continue;
        };
        for (name, event) in pairs {
            if name == "base" {
                continue;
            }
            let (vsnd, inherited_from) = resolve_clips(
                event_own_clips(event),
                event.get("base").and_then(Value::as_str),
                &base_table,
            );
            if vsnd.is_empty() {
                continue;
            }
            let segs: Vec<&str> = name.split('.').filter(|s| !s.is_empty()).collect();
            sounds.push(GlobalSound {
                inherited_from,
                label: prettify_segments(&segs),
                duration: event.get("vsnd_duration").and_then(Value::as_f64),
                soundevents: entry.clone(),
                source: source.clone(),
                category,
                vsnd,
                event: name.clone(),
            });
        }
    }

    sounds.sort_by(|a, b| a.source.cmp(&b.source).then_with(|| a.event.cmp(&b.event)));
    Ok(sounds)
}

/// Source name from a global soundevents entry path: the path relative to
/// `soundevents/`, minus the extension. `soundevents/npc/troopers.vsndevts_c`
/// -> `npc/troopers`; `soundevents/ui.vsndevts_c` -> `ui`.
fn global_source_name(entry: &str) -> String {
    entry
        .strip_prefix(SOUNDEVENTS_PREFIX)
        .unwrap_or(entry)
        .strip_suffix(".vsndevts_c")
        .unwrap_or(entry)
        .to_owned()
}

/// Category for a global source name. Matched on the source (the game's own
/// grouping) rather than the event name, which has no cross-file convention.
fn classify_global_source(source: &str) -> GlobalSoundCategory {
    // Tree prefixes first: everything under these directories shares a family.
    if let Some((dir, _)) = source.split_once('/') {
        match dir {
            "ambience" => return GlobalSoundCategory::Ambience,
            "npc" => return GlobalSoundCategory::Npc,
            "mods" => return GlobalSoundCategory::Item,
            _ => {}
        }
    }
    // `base/*` holds shared layers whose family is the leaf name.
    let leaf = source.rsplit('/').next().unwrap_or(source);
    match leaf {
        "ui" | "chat_wheel" => GlobalSoundCategory::Ui,
        "music" | "music_arpeggiator" => GlobalSoundCategory::Music,
        "ambience" | "world_ambient_emitters" => GlobalSoundCategory::Ambience,
        "damage"
        | "status_effects"
        | "powerups"
        | "gameplay"
        | "player"
        | "ziplines"
        | "breakables"
        | "utility"
        | "world"
        | "cosmetics"
        | "citadel_base_events"
        | "weapons_shared" => GlobalSoundCategory::Gameplay,
        // The generated pick/death VO and the new-player lines sit outside the
        // `vo/` tree but are still voice.
        s if s.contains("_vo") || s.starts_with("generated_vo") => GlobalSoundCategory::Voice,
        _ => GlobalSoundCategory::Other,
    }
}

/// Generic grouping segments that pin a non-ability category. Anything not in
/// these (after the speaker prefix and an optional `A1..A4` slot token) is read
/// as a named ability.
const WEAPON_GROUPS: &[&str] = &[
    "wpn",
    "weapon",
    "zoomin",
    "zoomout",
    "bulletwhizby",
    "whizby",
    "reload",
    "foley",
];
const MOVEMENT_GROUPS: &[&str] = &[
    "footstep",
    "footsteps",
    "movement",
    "jumpland",
    "jump",
    "land",
    "mantle",
    "ladderstep",
    "sprint",
    "slide",
    "dash",
    "rope",
    "zipline",
    "wallrun",
];
const MELEE_GROUPS: &[&str] = &["melee"];
/// Progression-page stingers (`Atlas.Progession.Page.Win.VO`); the in-game key is
/// misspelled `Progession`, so match both spellings.
const OTHER_GROUPS: &[&str] = &["progession", "progression"];

struct ClassifiedEvent {
    category: HeroSoundCategory,
    ability: Option<String>,
    slot: Option<u8>,
    label: String,
}

/// Classify a hero soundevent name into a category + ability/slot + label.
///
/// Events are namespaced `<Speaker>.<Group>.<detail...>`, where the speaker is a
/// hero prefix that can differ from the file stem (Gigawatt's file mixes `Seven.*`
/// and `Gigawatt.*`). A leading literal `Ability.` and an `A1..A4` slot token are
/// peeled first; the grouping segment then decides the category (a known generic,
/// else a named ability). The label is the prose remainder after the speaker.
fn classify_hero_event(event: &str) -> ClassifiedEvent {
    let segs: Vec<&str> = event.split('.').filter(|s| !s.is_empty()).collect();
    // segs[0] is the speaker; a few events lead with a literal `Ability` first.
    let mut i = usize::from(segs.first() == Some(&"Ability"));
    i += 1; // skip the speaker prefix itself
    let after_speaker = &segs[i.min(segs.len())..];

    // Peel an A1..A4 slot token if present (Abrams). The ability name then follows.
    let mut slot = None;
    let mut rest = after_speaker;
    if let Some(first) = rest.first() {
        if first.len() == 2
            && first.as_bytes()[0] == b'A'
            && (b'1'..=b'4').contains(&first.as_bytes()[1])
        {
            slot = Some(first.as_bytes()[1] - b'0');
            rest = &rest[1..];
        }
    }

    let group = rest.first().copied();
    // The label is the detail *after* the grouping segment, since the category
    // (and, for abilities, the ability name) already convey the group. When there
    // is no detail, fall back to the group's own pretty name.
    let detail = rest.get(1..).unwrap_or(&[]);
    let label = if detail.is_empty() {
        prettify_segment(group.unwrap_or(event))
    } else {
        prettify_segments(detail)
    };

    let Some(group) = group else {
        // No grouping segment at all (a bare speaker event); treat as Other.
        return ClassifiedEvent {
            category: HeroSoundCategory::Other,
            ability: None,
            slot,
            label,
        };
    };
    let g = group.to_ascii_lowercase();

    let category = if WEAPON_GROUPS.contains(&g.as_str()) {
        HeroSoundCategory::Weapon
    } else if MOVEMENT_GROUPS.contains(&g.as_str()) {
        HeroSoundCategory::Movement
    } else if MELEE_GROUPS.contains(&g.as_str()) {
        HeroSoundCategory::Melee
    } else if OTHER_GROUPS.contains(&g.as_str()) {
        HeroSoundCategory::Other
    } else {
        // A named group (or one proved an ability by its A1..A4 slot token).
        HeroSoundCategory::Ability
    };

    let ability = if category == HeroSoundCategory::Ability {
        Some(prettify_segment(group))
    } else {
        None
    };

    ClassifiedEvent {
        category,
        ability,
        slot,
        label,
    }
}

/// Recover an ability slot (1..4) from a clip path's `aN` token: a folder
/// `.../a2_batblink/...`, a bare `.../a4/...`, or an infix `..._a1_...`. Returns
/// the first match across `paths`. Mirrors the path convention the Locker's
/// per-ability sound classifier relies on.
fn slot_from_vsnd(paths: &[String]) -> Option<u8> {
    for path in paths {
        let lower = path.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        // Scan for `a` preceded by a `/` or `_` boundary, followed by 1..4 and a
        // `_` or `/` boundary.
        for i in 0..bytes.len() {
            if bytes[i] != b'a' {
                continue;
            }
            let before_ok = i == 0 || bytes[i - 1] == b'/' || bytes[i - 1] == b'_';
            if !before_ok {
                continue;
            }
            let Some(&digit) = bytes.get(i + 1) else {
                continue;
            };
            if !(b'1'..=b'4').contains(&digit) {
                continue;
            }
            let after = bytes.get(i + 2).copied();
            if matches!(after, None | Some(b'_' | b'/')) {
                return Some(digit - b'0');
            }
        }
    }
    None
}

/// Prettify one event segment: split CamelCase (`LoveBites` -> `Love Bites`,
/// `ZoomIn` -> `Zoom In`) and underscores (`Grasshopper_Lp` -> `Grasshopper Lp`,
/// `Impact_wall` -> `Impact Wall`), then Title-case the words. `Wpn` reads as
/// `Weapon`.
///
/// Both conventions appear, sometimes in the same segment: event authors use
/// CamelCase for the namespaced part and `snake_case` for the clip-ish suffixes
/// (`_lp` for a loop, `_end`, `_01`). Splitting on only one of them leaves the
/// other verbatim in the UI.
fn prettify_segment(seg: &str) -> String {
    if seg.eq_ignore_ascii_case("wpn") {
        return "Weapon".to_owned();
    }
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in seg.chars() {
        // An underscore is a word boundary, not a character: close the current
        // word and drop the separator itself.
        if ch == '_' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .iter()
        .map(|w| title_case_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prettify a run of segments, joining with spaces (`["Dagger", "Cast"]` ->
/// `"Dagger Cast"`). A leading `Wpn` is spelled `Weapon`.
fn prettify_segments(segs: &[&str]) -> String {
    segs.iter()
        .map(|s| prettify_segment(s))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Upper-case the first character, lower-case the rest (`fire` -> `Fire`,
/// `BulletWhizby` after a split is already one word). Leaves all-caps short tokens
/// like `Lp` readable.
fn title_case_word(w: &str) -> String {
    let mut chars = w.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_canonical_check_value() {
        // The standard CRC-32 check vector.
        assert_eq!(caption_hash("123456789"), 0xCBF4_3926);
    }

    /// Build a minimal valid VCCD v2 blob with two tokens and parse it back.
    #[test]
    fn parses_and_resolves_tokens() {
        let block_size = 8192u32;
        let tok_a = "hero_ability_cast_01";
        let tok_b = "hero_self_death_01";
        let text_a = "Witness my power!";
        let text_b = ""; // empty caption, must be dropped

        let enc = |s: &str| -> Vec<u8> {
            let mut v: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
            v.extend_from_slice(&[0, 0]); // NUL terminator
            v
        };
        let data_a = enc(text_a);
        let data_b = enc(text_b);

        let header_len = 24u32;
        let dir_len = 2 * 12;
        let data_offset = header_len + dir_len;
        let len_a = u16::try_from(data_a.len()).unwrap();
        let len_b = u16::try_from(data_b.len()).unwrap();

        let mut buf = Vec::new();
        buf.extend_from_slice(b"VCCD");
        buf.extend_from_slice(&2u32.to_le_bytes()); // version
        buf.extend_from_slice(&1u32.to_le_bytes()); // numBlocks
        buf.extend_from_slice(&block_size.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes()); // dirEntries
        buf.extend_from_slice(&data_offset.to_le_bytes());

        // Directory: hash, blockNum, offset, length.
        buf.extend_from_slice(&caption_hash(tok_a).to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&len_a.to_le_bytes());

        buf.extend_from_slice(&caption_hash(tok_b).to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&len_a.to_le_bytes());
        buf.extend_from_slice(&len_b.to_le_bytes());

        // Data block.
        buf.extend_from_slice(&data_a);
        buf.extend_from_slice(&data_b);

        let db = CaptionDb::parse(&buf).expect("parse VCCD");
        assert_eq!(db.get(tok_a), Some(text_a));
        assert_eq!(db.for_event(tok_a), Some(text_a));
        // Empty caption resolves to None.
        assert_eq!(db.get(tok_b), None);
        // Unknown token.
        assert_eq!(db.get("no_such_event"), None);
        assert_eq!(db.non_empty_count(), 1);
    }

    #[test]
    fn label_is_searchable_prose() {
        assert_eq!(
            pretty_label("bebop_ally_atlas_killed_in_lane_01_hero_3d", Some("bebop")),
            "ally atlas killed in lane"
        );
        assert_eq!(
            pretty_label("bebop_self_ultimate_cast_01_hero_3d", Some("bebop")),
            "self ultimate cast"
        );
        assert_eq!(
            pretty_label(
                "bebop_ally_warden_killed_in_lane_01_alt_01_hero_3d",
                Some("bebop")
            ),
            "ally warden killed in lane"
        );
        // No hero prefix to strip (announcer-style).
        assert_eq!(pretty_label("round_start_01", None), "round start");
    }

    #[test]
    fn hero_parsing_from_vo_path() {
        assert_eq!(
            hero_from_vo_entry("soundevents/vo/generated_vo_hero_bebop.vsndevts_c").as_deref(),
            Some("bebop")
        );
        assert_eq!(
            hero_from_vo_entry("soundevents/vo/announcer.vsndevts_c"),
            None
        );
    }

    #[test]
    fn global_source_name_strips_prefix_and_extension() {
        assert_eq!(global_source_name("soundevents/ui.vsndevts_c"), "ui");
        assert_eq!(
            global_source_name("soundevents/npc/troopers.vsndevts_c"),
            "npc/troopers"
        );
    }

    #[test]
    fn global_sources_classify_by_tree_then_leaf() {
        use GlobalSoundCategory as C;
        // Tree prefixes win, whatever the leaf is called.
        assert_eq!(classify_global_source("ambience/lanes"), C::Ambience);
        assert_eq!(classify_global_source("npc/troopers"), C::Npc);
        assert_eq!(classify_global_source("mods/weapon"), C::Item);
        // `base/*` and the loose files classify on the leaf name.
        assert_eq!(classify_global_source("base/ui"), C::Ui);
        assert_eq!(classify_global_source("ui"), C::Ui);
        assert_eq!(classify_global_source("music_arpeggiator"), C::Music);
        assert_eq!(classify_global_source("base/ambience"), C::Ambience);
        assert_eq!(classify_global_source("base/weapons_shared"), C::Gameplay);
        assert_eq!(classify_global_source("damage"), C::Gameplay);
        // VO that lives outside the `vo/` tree is still voice.
        assert_eq!(classify_global_source("new_player_vo"), C::Voice);
        assert_eq!(
            classify_global_source("generated_vo_hero_pick_vo"),
            C::Voice
        );
        // Dev/tools files fall through.
        assert_eq!(classify_global_source("soundevents_from_tools"), C::Other);
    }

    #[test]
    fn rejects_non_vccd() {
        assert!(CaptionDb::parse(b"not a caption db").is_err());
    }

    #[test]
    fn hero_from_hero_entry_strips_stem_and_skips_shared() {
        assert_eq!(
            hero_from_hero_entry("soundevents/hero/abrams.vsndevts_c").as_deref(),
            Some("abrams")
        );
        assert_eq!(
            hero_from_hero_entry("soundevents/hero/_shared.vsndevts_c"),
            None
        );
    }

    #[test]
    fn classifies_weapon_movement_melee() {
        let w = classify_hero_event("Haze.Wpn.Fire.Main");
        assert_eq!(w.category, HeroSoundCategory::Weapon);
        assert_eq!(w.label, "Fire Main");
        assert_eq!(w.ability, None);

        // Bare weapon token, no detail segments.
        let z = classify_hero_event("Seven.ZoomIn");
        assert_eq!(z.category, HeroSoundCategory::Weapon);
        assert_eq!(z.label, "Zoom In");

        assert_eq!(
            classify_hero_event("Haze.Footstep").category,
            HeroSoundCategory::Movement
        );
        assert_eq!(
            classify_hero_event("Abrams.Melee.Swing.Charged").category,
            HeroSoundCategory::Melee
        );
        assert_eq!(
            classify_hero_event("Atlas.Progession.Page.Win.VO").category,
            HeroSoundCategory::Other
        );
    }

    fn table(rows: &[(&str, &[&str], Option<&str>)]) -> BaseTable {
        rows.iter()
            .map(|(name, clips, base)| {
                (
                    (*name).to_owned(),
                    (
                        clips.iter().map(|c| (*c).to_owned()).collect(),
                        base.map(str::to_owned),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn own_clips_win_and_are_not_marked_inherited() {
        let t = table(&[("Base.Movement", &["b.vsnd"], None)]);
        let (clips, from) = resolve_clips(vec!["own.vsnd".to_owned()], Some("Base.Movement"), &t);
        assert_eq!(clips, vec!["own.vsnd".to_owned()]);
        assert_eq!(from, None);
    }

    #[test]
    fn clipless_event_inherits_its_template() {
        // The Abrams.Wpn.Whizby shape: no own clips, one hop to a template.
        let t = table(&[("Base.Bullet.Whizby.Shotgun", &["w1.vsnd", "w2.vsnd"], None)]);
        let (clips, from) = resolve_clips(Vec::new(), Some("Base.Bullet.Whizby.Shotgun"), &t);
        assert_eq!(clips.len(), 2);
        assert_eq!(from.as_deref(), Some("Base.Bullet.Whizby.Shotgun"));
    }

    #[test]
    fn walks_a_multi_link_chain_to_the_first_template_with_clips() {
        // The real Abrams.Wpn.Impact shape, which no single hop resolves:
        // Base.Bullet.Impact.Shotgun carries only parameters and defers again.
        let t = table(&[
            (
                "Base.Bullet.Impact.Shotgun",
                &[],
                Some("Base.Bullet.Impact"),
            ),
            ("Base.Bullet.Impact", &["i1.vsnd"], None),
        ]);
        let (clips, from) = resolve_clips(Vec::new(), Some("Base.Bullet.Impact.Shotgun"), &t);
        assert_eq!(clips, vec!["i1.vsnd".to_owned()]);
        // Credit the template that actually supplied the clips, not the first hop.
        assert_eq!(from.as_deref(), Some("Base.Bullet.Impact"));
    }

    #[test]
    fn a_cyclic_chain_terminates_instead_of_hanging() {
        let t = table(&[("A", &[], Some("B")), ("B", &[], Some("A"))]);
        let (clips, from) = resolve_clips(Vec::new(), Some("A"), &t);
        assert!(clips.is_empty());
        assert_eq!(from, None);
    }

    #[test]
    fn a_dangling_template_reference_resolves_to_nothing() {
        let t = table(&[("Real", &["r.vsnd"], None)]);
        let (clips, from) = resolve_clips(Vec::new(), Some("Missing"), &t);
        assert!(clips.is_empty());
        assert_eq!(from, None);
    }

    #[test]
    fn an_event_with_neither_clips_nor_base_stays_unplayable() {
        let (clips, from) = resolve_clips(Vec::new(), None, &table(&[]));
        assert!(clips.is_empty());
        assert_eq!(from, None);
    }

    #[test]
    fn a_chain_longer_than_the_cap_gives_up_rather_than_spinning() {
        // Each link defers to the next; the payload sits past MAX_BASE_DEPTH.
        let names: Vec<String> = (0..MAX_BASE_DEPTH + 3).map(|i| format!("L{i}")).collect();
        let mut t: BaseTable = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            let next = names.get(i + 1).cloned();
            t.insert(name.clone(), (Vec::new(), next));
        }
        let last = names.last().unwrap().clone();
        t.insert(last, (vec!["deep.vsnd".to_owned()], None));
        let (clips, _) = resolve_clips(Vec::new(), Some("L0"), &t);
        assert!(
            clips.is_empty(),
            "should bail at the depth cap, not walk on"
        );
    }

    #[test]
    fn single_string_vsnd_files_normalizes_to_a_one_element_pool() {
        let event = Value::Object(vec![(
            "vsnd_files".to_owned(),
            Value::String("solo.vsnd".to_owned()),
        )]);
        assert_eq!(event_own_clips(&event), vec!["solo.vsnd".to_owned()]);
    }

    #[test]
    fn classifies_named_and_slotted_abilities() {
        // Named ability: the grouping segment is the ability.
        let n = classify_hero_event("Haze.Finesse.Dagger.Cast");
        assert_eq!(n.category, HeroSoundCategory::Ability);
        assert_eq!(n.ability.as_deref(), Some("Finesse"));
        assert_eq!(n.label, "Dagger Cast");
        assert_eq!(n.slot, None);

        // CamelCase ability name splits into words.
        assert_eq!(
            classify_hero_event("VampireBat.LoveBites.Buildup")
                .ability
                .as_deref(),
            Some("Love Bites")
        );

        // A1..A4 slot token is peeled; the ability name follows.
        let s = classify_hero_event("Abrams.A1.SiphonLife.Cast");
        assert_eq!(s.category, HeroSoundCategory::Ability);
        assert_eq!(s.slot, Some(1));
        assert_eq!(s.ability.as_deref(), Some("Siphon Life"));
        assert_eq!(s.label, "Cast");

        // Leading literal `Ability.` prefix is skipped.
        let a = classify_hero_event("Ability.Abrams.Charge.Step");
        assert_eq!(a.category, HeroSoundCategory::Ability);
        assert_eq!(a.ability.as_deref(), Some("Charge"));
    }

    #[test]
    fn prettify_splits_underscores_as_well_as_camelcase() {
        // snake_case suffixes are as common as CamelCase in event names, and
        // often share a segment with it.
        assert_eq!(prettify_segment("Grasshopper_Lp"), "Grasshopper Lp");
        assert_eq!(prettify_segment("Impact_wall"), "Impact Wall");
        assert_eq!(prettify_segment("Park_walla_lp"), "Park Walla Lp");
        assert_eq!(prettify_segment("IcePath_end"), "Ice Path End");
        // Repeated / edge separators collapse rather than emitting empty words.
        assert_eq!(prettify_segment("_Full__start_"), "Full Start");
        // CamelCase-only and plain segments are unchanged.
        assert_eq!(prettify_segment("LoveBites"), "Love Bites");
        assert_eq!(prettify_segment("Cast"), "Cast");
        // The `Wpn` special case still short-circuits.
        assert_eq!(prettify_segment("Wpn"), "Weapon");
    }

    #[test]
    fn hero_labels_carry_the_underscore_split_too() {
        // The same helper backs hero and global labels, so a snake_case hero
        // detail reads as prose instead of a raw token.
        assert_eq!(
            classify_hero_event("Bebop.Uppercut.Impact_wall").label,
            "Impact Wall"
        );
    }

    #[test]
    fn slot_recovered_from_clip_path() {
        assert_eq!(
            slot_from_vsnd(&["sounds/abilities/vampirebat/a2_batblink/x_01.vsnd".to_owned()]),
            Some(2)
        );
        assert_eq!(
            slot_from_vsnd(&["sounds/abilities/h/a4/ult_01.vsnd".to_owned()]),
            Some(4)
        );
        // No aN token anywhere.
        assert_eq!(
            slot_from_vsnd(&["sounds/weapons/abrams/fire_01.vsnd".to_owned()]),
            None
        );
    }
}
