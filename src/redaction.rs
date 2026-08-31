pub const REDACTED: &str = "[REDACTED]";

/// Removes exact secret values at the boundary where provider diagnostics
/// become user-visible or enter structured logs.
#[must_use]
pub fn redact_secret_values<'a>(
    value: String,
    secrets: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut ranges = Vec::new();
    for secret in secrets {
        if secret.chars().count() < 4 {
            continue;
        }
        ranges.extend(
            value
                .match_indices(secret)
                .map(|(start, _)| start..start + secret.len()),
        );
    }
    if ranges.is_empty() {
        return value;
    }

    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }

    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    for range in merged {
        output.push_str(&value[cursor..range.start]);
        output.push_str(REDACTED);
        cursor = range.end;
    }
    output.push_str(&value[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn azure_aws_and_mcp_tokens_are_removed_together() {
        let azure = "azure-fake-secret-42";
        let aws = "aws-fake-session-token-84";
        let mcp = "mcp-fake-bearer-21";
        let input = format!("azure={azure}; aws={aws}; Authorization: Bearer {mcp}");
        let output = redact_secret_values(input, [azure, aws, mcp]);

        assert_eq!(output.matches(REDACTED).count(), 3);
        for secret in [azure, aws, mcp] {
            assert!(!output.contains(secret));
        }
    }

    #[test]
    fn empty_and_tiny_values_are_not_global_replacement_patterns() {
        assert_eq!(
            redact_secret_values("normal diagnostic".to_owned(), ["", "a"]),
            "normal diagnostic"
        );
    }

    #[test]
    fn overlapping_secrets_are_redacted_without_leaking_a_suffix() {
        let output = redact_secret_values("token=abcdef".to_owned(), ["abcd", "abcdef", "abcd"]);

        assert_eq!(output, "token=[REDACTED]");
        assert!(!output.contains("ef"));
    }

    #[test]
    fn short_unicode_values_are_not_treated_as_secrets() {
        assert_eq!(
            redact_secret_values("status=éé".to_owned(), ["éé"]),
            "status=éé"
        );
    }
}
