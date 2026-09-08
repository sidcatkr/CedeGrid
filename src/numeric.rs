//! Validate numeric lexemes before serde_json can coerce overflowing integer tokens.
use anyhow::{Result, ensure};
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let raw: &RawValue = serde_json::from_slice(bytes)?;
    validate(raw, 0)?;
    Ok(serde_json::from_slice(bytes)?)
}
fn validate(raw: &RawValue, depth: usize) -> Result<()> {
    ensure!(depth <= 128, "ERR_CEDEGRID_ARGUMENT: JSON nesting limit");
    let token = raw.get();
    match token.as_bytes()[0] {
        b'{' => {
            let fields: std::collections::BTreeMap<String, &RawValue> =
                serde_json::from_str(token)?;
            for value in fields.values() {
                validate(value, depth + 1)?;
            }
        }
        b'[' => {
            let values: Vec<&RawValue> = serde_json::from_str(token)?;
            for value in values {
                validate(value, depth + 1)?;
            }
        }
        b'-' | b'0'..=b'9' => {
            if token == "-0" || token.contains(['.', 'e', 'E']) {
                let value: f64 = token.parse()?;
                ensure!(
                    value.is_finite(),
                    "ERR_CEDEGRID_ARGUMENT: nonfinite JSON number"
                );
                let significand = token.split(['e', 'E']).next().unwrap();
                let nonzero = significand.bytes().any(|b| matches!(b, b'1'..=b'9'));
                ensure!(
                    value != 0.0 || !nonzero,
                    "ERR_CEDEGRID_ARGUMENT: JSON decimal underflows to zero"
                );
            } else if token.starts_with('-') {
                token.parse::<i64>().map_err(|_| {
                    anyhow::anyhow!("ERR_CEDEGRID_ARGUMENT: JSON integer below i64 minimum")
                })?;
            } else {
                token.parse::<u64>().map_err(|_| {
                    anyhow::anyhow!("ERR_CEDEGRID_ARGUMENT: JSON integer above u64 maximum")
                })?;
            }
        }
        _ => {}
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_metadata_fixtures_execute_real_codec() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/contracts/cedegrid-0.2.json")).unwrap();
        for case in fixtures["metadata_cases"].as_array().unwrap() {
            let token = case["json"].as_str().unwrap();
            let parsed = from_slice::<serde_json::Value>(token.as_bytes());
            if case.get("error").is_some() {
                assert!(parsed.is_err(), "{}: {token}", case["id"]);
                continue;
            }
            let parsed = parsed.unwrap_or_else(|e| panic!("{}: {e}", case["id"]));
            let expected = &case["expected"];
            match expected["kind"].as_str() {
                Some("integer") => assert_eq!(
                    parsed.to_string(),
                    expected["decimal"].as_str().unwrap(),
                    "{}",
                    case["id"]
                ),
                Some("binary64") => assert_eq!(
                    format!("{:016x}", parsed.as_f64().unwrap().to_bits()),
                    expected["bits_hex"].as_str().unwrap(),
                    "{}",
                    case["id"]
                ),
                _ => {}
            }
            let wire = serde_json::to_vec(&parsed).unwrap();
            let reparsed: serde_json::Value = from_slice(&wire).unwrap();
            assert_eq!(parsed, reparsed, "{}", case["id"]);
        }
    }
}
