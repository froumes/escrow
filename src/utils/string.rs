/// String utility functions

/// Items that cannot stack in Minecraft/Hypixel SkyBlock (max stack size = 1).
const UNSTACKABLE_ITEM_NAMES: &[&str] = &[
    "enchanted book",
];

/// Item tag prefixes that indicate unstackable items.
const UNSTACKABLE_TAG_PREFIXES: &[&str] = &[
    "ENCHANTMENT_",
];

/// Returns true if the given bazaar item is unstackable (max stack size = 1).
/// Checks both the item name and the optional item tag.
pub fn is_unstackable_item(item_name: &str, item_tag: Option<&str>) -> bool {
    let lower = item_name.to_lowercase();
    if UNSTACKABLE_ITEM_NAMES.iter().any(|&n| lower.contains(n)) {
        return true;
    }
    if let Some(tag) = item_tag {
        if UNSTACKABLE_TAG_PREFIXES.iter().any(|&prefix| tag.starts_with(prefix)) {
            return true;
        }
    }
    false
}

/// Format a number with thousands separators
pub fn format_number_with_separators(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();
    
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(*ch);
    }
    
    result
}

/// Remove Minecraft color codes from text
/// Format: §x where x is a color code
pub fn remove_minecraft_colors(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars();
    
    while let Some(ch) = chars.next() {
        if ch == '§' || ch == '┬' {
            // Skip the next character (color code)
            chars.next();
        } else {
            result.push(ch);
        }
    }
    
    result
}

/// Synthetic status lines TWM emits as `[BAF]: …` (startup, cookie, buy timing, etc.).
/// Real Hypixel chat does not use this prefix after § codes are stripped.
pub fn is_twm_baf_synthetic_chat_line(msg: &str) -> bool {
    remove_minecraft_colors(msg)
        .trim_start()
        .starts_with("[BAF]:")
}

/// Convert string to title case
pub fn to_title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_baf_synthetic_line() {
        assert!(is_twm_baf_synthetic_chat_line(
            "§f[§4BAF§f]: §7[Startup] §bStep 1/4"
        ));
        assert!(is_twm_baf_synthetic_chat_line(
            "\u{00A7}f[\u{00A7}4BAF\u{00A7}f]: test"
        ));
        assert!(!is_twm_baf_synthetic_chat_line("§aYou purchased something for 1 coins!"));
        assert!(!is_twm_baf_synthetic_chat_line("[Auction] Steve bought dirt for 1 coins"));
    }

        assert_eq!(format_number_with_separators(1000), "1,000");
        assert_eq!(format_number_with_separators(1000000), "1,000,000");
        assert_eq!(format_number_with_separators(123), "123");
    }

    #[test]
    fn test_remove_minecraft_colors() {
        assert_eq!(
            remove_minecraft_colors("§aGreen§r Text"),
            "Green Text"
        );
        assert_eq!(
            remove_minecraft_colors("§6Buy Item Right Now"),
            "Buy Item Right Now"
        );
    }

    #[test]
    fn test_to_title_case() {
        assert_eq!(to_title_case("hello world"), "Hello World");
        assert_eq!(to_title_case("HELLO WORLD"), "Hello World");
        assert_eq!(to_title_case("hello"), "Hello");
    }

    #[test]
    fn test_is_unstackable_item_enchanted_book_name() {
        assert!(is_unstackable_item("Enchanted Book", None));
        assert!(is_unstackable_item("enchanted book", None));
        assert!(is_unstackable_item("Enchanted Book of Power", None));
    }

    #[test]
    fn test_is_unstackable_item_enchantment_tag() {
        assert!(is_unstackable_item("Enchanted Book", Some("ENCHANTMENT_ICE_COLD_1")));
        assert!(is_unstackable_item("Some Item", Some("ENCHANTMENT_SHARPNESS_6")));
    }

    #[test]
    fn test_is_unstackable_item_stackable() {
        assert!(!is_unstackable_item("Enchanted Diamond", None));
        assert!(!is_unstackable_item("Enchanted Raw Salmon", Some("ENCHANTED_RAW_SALMON")));
        assert!(!is_unstackable_item("Rough Peridot Gemstone", None));
    }
}
