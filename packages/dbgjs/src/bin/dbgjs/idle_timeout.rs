use std::io;
use std::time::{Duration, Instant};

use dbgjs::api::service_api::IdleTimeout;

pub(super) fn extract_idle_timeout_option(
    arguments: &mut Vec<String>,
) -> Result<Option<Option<IdleTimeout>>, io::Error> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Ok(None);
    };
    let is_connection = command == "connection";
    if !(is_connection || command == "context")
        || !arguments
            .get(1)
            .is_some_and(|operation| matches!(operation.as_str(), "create" | "add" | "configure"))
    {
        return Ok(None);
    }
    let mut timeout = None;
    let mut index = 2;
    while index < arguments.len() {
        if arguments[index] == "--" {
            break;
        }
        if arguments[index] != "--idle-timeout" {
            index += if matches!(
                arguments[index].as_str(),
                "--arg"
                    | "--runtime-arg"
                    | "--node"
                    | "--playwright"
                    | "--chrome"
                    | "--node-inspector"
                    | "--process"
                    | "--process-tree"
                    | "--env"
                    | "--cwd"
                    | "--runtime-executable"
                    | "--executable"
                    | "--channel"
                    | "--user-data-dir"
            ) {
                2
            } else {
                1
            };
            continue;
        }
        if timeout.is_some() {
            return Err(invalid("--idle-timeout may only be specified once"));
        }
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| invalid("--idle-timeout requires a duration, inf, or inherit"))?;
        let parsed = if value == "inherit" && is_connection {
            None
        } else {
            Some(parse_idle_timeout(value)?)
        };
        timeout = Some(parsed);
        arguments.drain(index..index + 2);
    }
    Ok(timeout)
}

fn parse_idle_timeout(value: &str) -> Result<IdleTimeout, io::Error> {
    if value == "inf" {
        return Ok(IdleTimeout::Infinite);
    }
    let (number, multiplier) = [
        ("ms", 1u64),
        ("s", 1_000),
        ("m", 60_000),
        ("h", 3_600_000),
        ("d", 86_400_000),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        value
            .strip_suffix(suffix)
            .map(|number| (number, multiplier))
    })
    .ok_or_else(|| {
        invalid("idle timeout must be inf or a positive integer with ms, s, m, h, or d units")
    })?;
    let milliseconds = number
        .parse::<u64>()
        .ok()
        .filter(|_| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|number| number.checked_mul(multiplier))
        .filter(|milliseconds| {
            *milliseconds > 0
                && Instant::now()
                    .checked_add(Duration::from_millis(*milliseconds))
                    .is_some()
        })
        .ok_or_else(|| invalid("idle timeout must be a positive, representable duration"))?;
    Ok(IdleTimeout::After { milliseconds })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_timeout_parses_units_and_rejects_invalid_values() {
        for (text, milliseconds) in [
            ("1ms", 1),
            ("2s", 2_000),
            ("30m", 1_800_000),
            ("2h", 7_200_000),
            ("1d", 86_400_000),
        ] {
            assert_eq!(
                parse_idle_timeout(text).unwrap(),
                IdleTimeout::After { milliseconds }
            );
        }
        assert_eq!(parse_idle_timeout("inf").unwrap(), IdleTimeout::Infinite);
        for text in [
            "",
            "0s",
            "-1s",
            "+1s",
            "1.5h",
            "2",
            "never",
            "inherit",
            "18446744073709551615h",
        ] {
            assert!(parse_idle_timeout(text).is_err(), "{text}");
        }
    }

    #[test]
    fn idle_timeout_extraction_preserves_provider_arguments() {
        let mut arguments = [
            "connection",
            "add",
            "--node",
            "app.js",
            "--arg",
            "--idle-timeout",
            "--idle-timeout",
            "inherit",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            extract_idle_timeout_option(&mut arguments).unwrap(),
            Some(None)
        );
        assert_eq!(
            arguments,
            [
                "connection",
                "add",
                "--node",
                "app.js",
                "--arg",
                "--idle-timeout"
            ]
        );
        for values in [
            vec!["context", "create", ":test", "--idle-timeout", "inherit"],
            vec!["context", "configure", "--idle-timeout"],
            vec![
                "connection",
                "configure",
                "--idle-timeout",
                "inf",
                "--idle-timeout",
                "1h",
            ],
        ] {
            assert!(
                extract_idle_timeout_option(&mut values.into_iter().map(str::to_owned).collect())
                    .is_err()
            );
        }
    }
}
