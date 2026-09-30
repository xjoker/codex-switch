/// Format the backend's raw credit balance for display without implying a
/// currency conversion. The shortest round-trippable numeric representation is
/// retained, except that integral balances omit a redundant decimal suffix.
pub fn format_credits_balance(balance: f64) -> String {
    if !balance.is_finite() {
        return "unknown credits".into();
    }
    let formatted = balance.to_string();
    if formatted.contains('e') || formatted.contains('E') {
        return format!("{formatted} credits");
    }
    let (integer, fraction) = formatted
        .split_once('.')
        .unwrap_or((formatted.as_str(), ""));
    let (sign, digits) = integer
        .strip_prefix('-')
        .map(|digits| ("-", digits))
        .unwrap_or(("", integer));
    let mut grouped = String::with_capacity(formatted.len() + digits.len() / 3);
    grouped.push_str(sign);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if fraction.is_empty() {
        format!("{grouped} credits")
    } else {
        format!("{grouped}.{fraction} credits")
    }
}

#[cfg(test)]
mod tests {
    use super::format_credits_balance;

    #[test]
    fn formats_raw_credit_units_without_currency() {
        assert_eq!(format_credits_balance(62_500.0), "62,500 credits");
        assert_eq!(format_credits_balance(15.5), "15.5 credits");
        assert_eq!(format_credits_balance(1.234_567), "1.234567 credits");
        assert_eq!(format_credits_balance(0.0), "0 credits");
        assert_eq!(format_credits_balance(-1_234.5), "-1,234.5 credits");
    }
}
