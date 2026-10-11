//! Exact three-part versions (REQ-001). Ranges like `24` or `lts` are rejected.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        let t = t.strip_prefix('v').unwrap_or(t);
        let parts: Vec<&str> = t.split('.').collect();
        let bad = || {
            crate::tr!(
                "`{}` は版をそのまま書いた形(例: 24.11.1)ではありません",
                "`{}` is not an exact version like 24.11.1",
                s.trim()
            )
        };
        if parts.len() != 3 {
            return Err(bad());
        }
        let mut nums = [0u64; 3];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            nums[i] = p.parse().map_err(|_| bad())?;
        }
        Ok(Version {
            major: nums[0],
            minor: nums[1],
            patch: nums[2],
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exact_versions() {
        assert_eq!("24.11.1".parse::<Version>().unwrap().to_string(), "24.11.1");
        assert_eq!(
            "v22.0.10\n".parse::<Version>().unwrap().to_string(),
            "22.0.10"
        );
    }

    #[test]
    fn rejects_ranges_and_aliases() {
        for s in [
            "24",
            "24.11",
            "lts",
            "latest",
            "24.x.1",
            "24.11.1-rc1",
            "",
            "24.11.1.0",
        ] {
            assert!(s.parse::<Version>().is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn orders_numerically() {
        let a: Version = "9.10.0".parse().unwrap();
        let b: Version = "10.2.0".parse().unwrap();
        assert!(a < b);
    }
}
