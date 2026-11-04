//! Pure gamelog line classification.
//!
//! EVE localizes message bodies per client language but keeps the
//! category tag — `(combat)`, `(notify)`, `(question)`, `(None)` —
//! in English everywhere. So each check gates on the English tag and
//! then matches any language's body phrase from
//! `assets/eve_log_patterns.json`.
//!
//! The phrase table was extracted from EVE's own client localization
//! files by the EVE-MultiPreview project
//! (https://github.com/CJKondur/EVE-MultiPreview, MIT) and is reused
//! here with thanks.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::alerts::AlertKind;

fn patterns() -> &'static HashMap<String, Vec<String>> {
    static P: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    P.get_or_init(|| {
        serde_json::from_str(include_str!("../../assets/eve_log_patterns.json"))
            .expect("bundled eve_log_patterns.json parses")
    })
}

fn get(key: &str) -> &'static [String] {
    patterns().get(key).map(Vec::as_slice).unwrap_or(&[])
}

fn matches(line: &str, key: &str) -> bool {
    get(key).iter().any(|p| line.contains(p.as_str()))
}

/// Character name from a gamelog header line such as
/// `  Listener: Some Pilot` (or `收听者：Some Pilot` on a Chinese
/// client, which also uses a full-width colon). None if the line
/// isn't a header.
pub fn parse_listener(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches('\u{feff}').trim_start();
    for key in get("log_header_keys") {
        let Some(rest) = trimmed.strip_prefix(key.as_str()) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：')) else {
            continue;
        };
        let name = rest.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// Classify one gamelog line. None = nothing alert-worthy.
pub fn classify(line: &str) -> Option<AlertKind> {
    // Non-English clients wrap proper nouns (module names, ships) in
    // `<localized hint="…">…</localized>`. Resolve to the English side
    // so the mining-module gate below works in every language.
    let resolved = resolve_localized_names(line);
    let line = resolved.as_str();

    if line.contains("(combat)") {
        return classify_combat(line);
    }

    if line.contains("(notify)") {
        if matches(line, "warp_scramble") {
            return Some(AlertKind::WarpScrambled);
        }
        if matches(line, "decloak") {
            return Some(AlertKind::Decloaked);
        }
        // Checked in this order because the specific mining stop
        // messages share wording with the generic target-lost one.
        if matches(line, "mining_cargo_full") {
            return Some(AlertKind::CargoFull);
        }
        if matches(line, "mining_depleted") {
            return Some(AlertKind::AsteroidDepleted);
        }
        if matches(line, "mining_crystal_broken") {
            return Some(AlertKind::CrystalBroken);
        }
        // "X deactivates because its target, Y, is no longer present"
        // is shared by every targeted module — only treat it as a
        // mining event when the module is a miner.
        if matches(line, "mining_target_lost") && is_mining_module(line) {
            return Some(AlertKind::AsteroidDepleted);
        }
        return None;
    }

    // A Chinese client writes this scramble notice untagged.
    if line.contains("试图跃迁扰频") {
        return Some(AlertKind::WarpScrambled);
    }

    if (line.contains("(question)") && matches(line, "fleet_invite"))
        || line.contains("邀请你加入舰队")
    {
        return Some(AlertKind::FleetInvite);
    }

    if line.contains("(None)") && matches(line, "convo_request") {
        return Some(AlertKind::ConvoRequest);
    }

    None
}

fn classify_combat(line: &str) -> Option<AlertKind> {
    // Incoming damage is a red number: `<color=0xffcc0000><b>123</b>`.
    // Outgoing damage is cyan (0xff00ffff) and logi is 0xffccff66, so
    // the colour alone tells direction.
    if has_incoming_damage(line) {
        return Some(AlertKind::Attacked);
    }
    if matches(line, "combat_miss") {
        return Some(AlertKind::Attacked);
    }

    // Module tackle is a (combat) line, not a (notify) one:
    //   "Warp scramble attempt from <pilot's ship> to you!"
    // Scrambling someone else logs "from you to <target>" — never alert
    // on that.
    // Collapse the whitespace left behind by stripped tags so
    // "to</font> <b>you" reads as "to you".
    let plain = strip_tags(line)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if get("warp_scramble_combat")
        .iter()
        .any(|p| plain.contains(p.as_str()))
    {
        let incoming = contains_word(&plain, "to you");
        let outgoing = contains_word(&plain, "from you");
        if incoming && !outgoing {
            return Some(AlertKind::WarpScrambled);
        }
    }
    None
}

fn has_incoming_damage(line: &str) -> bool {
    const RED: &str = "<color=0xffcc0000><b>";
    let lower = line.to_ascii_lowercase();
    let mut search = lower.as_str();
    while let Some(i) = search.find(RED) {
        let after = &search[i + RED.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with("</b>") {
            return true;
        }
        search = after;
    }
    false
}

fn is_mining_module(line: &str) -> bool {
    line.contains("Miner ")
        || line.contains("Mining Laser")
        || line.contains("Harvester")
        || matches(line, "mining_module_names")
}

/// `needle` occurs in `hay` followed by a non-alphanumeric char (or
/// end of string). Stops "to you" matching "to your".
fn contains_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        hay[i + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric())
    })
}

/// Replace every `<...>` markup tag with a space.
fn strip_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_tag = false;
    for c in line.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Rewrite `<localized hint="A">B*</localized>` to whichever of A/B is
/// plain ASCII (the English name). Which side carries English depends
/// on the player's "show item names in English" setting, so pick by
/// script rather than position; fall back to the hint.
fn resolve_localized_names(line: &str) -> String {
    const OPEN: &str = "<localized";
    if !line.contains(OPEN) {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let tag = &rest[start..];
        match parse_localized(tag) {
            Some((name, consumed)) => {
                out.push_str(name);
                rest = &tag[consumed..];
            }
            None => {
                out.push_str(OPEN);
                rest = &tag[OPEN.len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse one localized tag at the start of `tag`. Returns the chosen
/// name and how many bytes the tag spanned.
fn parse_localized(tag: &str) -> Option<(&str, usize)> {
    const HINT: &str = "hint=\"";
    let hint_start = tag.find(HINT)? + HINT.len();
    let gt = tag.find('>')?;
    if hint_start > gt {
        return None;
    }
    let hint_len = tag[hint_start..].find('"')?;
    let hint = &tag[hint_start..hint_start + hint_len];

    let body_start = gt + 1;
    let body_len = tag[body_start..]
        .find(['*', '<'])
        .unwrap_or(tag.len() - body_start);
    let body = &tag[body_start..body_start + body_len];

    let mut end = body_start + body_len;
    if tag[end..].starts_with('*') {
        end += 1;
    }
    if tag[end..].starts_with("</localized>") {
        end += "</localized>".len();
    }

    let hint_en = is_ascii_name(hint);
    let body_en = is_ascii_name(body);
    let name = if hint_en && !body_en {
        hint
    } else if (body_en && !hint_en) || hint.is_empty() {
        body
    } else {
        hint
    };
    Some((name, end))
}

fn is_ascii_name(s: &str) -> bool {
    s.is_ascii() && s.chars().any(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = "[ 2026.11.03 18:21:07 ]";

    fn l(rest: &str) -> String {
        format!("{} {}", TS, rest)
    }

    #[test]
    fn listener_header_english_and_chinese() {
        assert_eq!(
            parse_listener("  Listener: Some Pilot").as_deref(),
            Some("Some Pilot")
        );
        assert_eq!(
            parse_listener("  收听者：中文 名字").as_deref(),
            Some("中文 名字")
        );
        assert_eq!(parse_listener("  Session Started: 2026.11.03"), None);
        assert_eq!(parse_listener("  Gamelog"), None);
    }

    #[test]
    fn listener_header_tolerates_bom() {
        assert_eq!(
            parse_listener("\u{feff}  Listener: Pilot").as_deref(),
            Some("Pilot")
        );
    }

    #[test]
    fn cargo_full_english() {
        let line = l("(notify) Modulated Strip Miner II deactivates. Ship's cargo hold is full. ");
        // The real message varies by module; the pattern table carries
        // "cargo hold is full" as a catch-all.
        assert_eq!(classify(&line), Some(AlertKind::CargoFull));
        let line = l("(notify) Miner II has completed operations. Ship's cargo hold is full.");
        assert_eq!(classify(&line), Some(AlertKind::CargoFull));
    }

    #[test]
    fn cargo_full_german() {
        let line =
            l("(notify) Miner II hat Aktionen beendet. Der Frachtraum des Schiffs ist voll.");
        assert_eq!(classify(&line), Some(AlertKind::CargoFull));
    }

    #[test]
    fn asteroid_depleted() {
        let line = l("(notify) Miner II deactivates as it finds the resource it was harvesting a pale shadow of its former glory.");
        assert_eq!(classify(&line), Some(AlertKind::AsteroidDepleted));
    }

    #[test]
    fn crystal_broken() {
        let line = l("(notify) Modulated Strip Miner II deactivates due to the destruction of the Veldspar Mining Crystal II it was fitted with.");
        assert_eq!(classify(&line), Some(AlertKind::CrystalBroken));
    }

    #[test]
    fn target_lost_only_for_mining_modules() {
        let mining =
            l("(notify) Miner II deactivates because its target, Veldspar, is no longer present.");
        assert_eq!(classify(&mining), Some(AlertKind::AsteroidDepleted));
        let tractor = l("(notify) Small Tractor Beam I deactivates because its target, Wreck, is no longer present.");
        assert_eq!(classify(&tractor), None);
    }

    #[test]
    fn target_lost_with_localized_module_name() {
        // Chinese client with the English name in the hint.
        let line = l("(notify) <localized hint=\"Miner II\">采矿器 II*</localized>未锁定，");
        assert_eq!(classify(&line), Some(AlertKind::AsteroidDepleted));
    }

    #[test]
    fn incoming_damage_is_attack() {
        let line = l("(combat) <color=0xffcc0000><b>212</b> <color=0x77ffffff><font size=10>from</font> <b><color=0xffffffff>Guristas Despoiler</b><font size=10><color=0x77ffffff> - Hits");
        assert_eq!(classify(&line), Some(AlertKind::Attacked));
    }

    #[test]
    fn outgoing_damage_and_logi_are_not_attacks() {
        let out = l("(combat) <color=0xff00ffff><b>350</b> <color=0x77ffffff><font size=10>to</font> <b><color=0xffffffff>Guristas Despoiler</b>");
        assert_eq!(classify(&out), None);
        let logi = l("(combat) <color=0xffccff66><b>240</b><color=0x77ffffff><font size=10> remote armor repaired to </font><b><color=0xffffffff>Friend</b>");
        assert_eq!(classify(&logi), None);
    }

    #[test]
    fn miss_is_attack() {
        let line = l("(combat) Guristas Despoiler misses you completely");
        assert_eq!(classify(&line), Some(AlertKind::Attacked));
    }

    #[test]
    fn bubble_scramble() {
        let line = l("(notify) You are within a warp disruption zone. Get 10000 meters from Warp Disrupt Probe to warp.");
        assert_eq!(classify(&line), Some(AlertKind::WarpScrambled));
    }

    #[test]
    fn module_scramble_incoming_only() {
        let incoming = l("(combat) <color=0xffffffff><b>Warp scramble attempt</b> <color=0x77ffffff><font size=10>from</font> <color=0xffffffff><b>Bad Guy's Sabre</b> <color=0x77ffffff><font size=10>to</font> <b><color=0xffffffff></font>you!");
        assert_eq!(classify(&incoming), Some(AlertKind::WarpScrambled));
        let outgoing =
            l("(combat) <b>Warp scramble attempt</b> from <b>you</b> to <b>Some Target</b>");
        assert_eq!(classify(&outgoing), None);
    }

    #[test]
    fn decloak() {
        let line = l("(notify) Your cloak deactivates due to proximity to a nearby object.");
        assert_eq!(classify(&line), Some(AlertKind::Decloaked));
    }

    #[test]
    fn fleet_invite_and_convo() {
        let invite = l("(question) <center>Bob wants you to join their fleet, do you accept?");
        assert_eq!(classify(&invite), Some(AlertKind::FleetInvite));
        let convo = l("(None) Bob is inviting you to a conversation.");
        assert_eq!(classify(&convo), Some(AlertKind::ConvoRequest));
    }

    #[test]
    fn tag_gating_prevents_false_positives() {
        // Same body text without the (notify) tag does nothing.
        let line = l("(None) cargo hold is full");
        assert_eq!(classify(&line), None);
        assert_eq!(
            classify(&l("(mining) You mined 1,234 units of Veldspar")),
            None
        );
        assert_eq!(
            classify(&l("(info) Undocking from Jita IV to Jita solar system.")),
            None
        );
    }

    #[test]
    fn localized_resolution_picks_english_side() {
        assert_eq!(
            resolve_localized_names("x <localized hint=\"Miner II\">采矿器*</localized> y"),
            "x Miner II y"
        );
        // English names enabled on a Russian client: body is English.
        assert_eq!(
            resolve_localized_names("<localized hint=\"Буровой лазер\">Miner II*</localized>"),
            "Miner II"
        );
        // Unterminated tag is left alone rather than panicking.
        assert_eq!(resolve_localized_names("a <localized b"), "a <localized b");
    }

    #[test]
    fn contains_word_respects_boundaries() {
        assert!(contains_word("x to you!", "to you"));
        assert!(contains_word("x to you", "to you"));
        assert!(!contains_word("x to your ship", "to you"));
    }
}
