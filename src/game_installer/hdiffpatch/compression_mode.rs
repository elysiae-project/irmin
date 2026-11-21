#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CompressionMode {
    #[default]
    Nocomp,
    Zstd,
    Zlib,
    Lz4,
}

impl std::str::FromStr for CompressionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("nocomp") || s.is_empty() {
            Ok(CompressionMode::Nocomp)
        } else if s.eq_ignore_ascii_case("zstd") {
            Ok(CompressionMode::Zstd)
        } else if s.eq_ignore_ascii_case("zlib") {
            Ok(CompressionMode::Zlib)
        } else if s.eq_ignore_ascii_case("lz4") {
            Ok(CompressionMode::Lz4)
        } else {
            Err(format!("unsupported compression mode: {s}"))
        }
    }
}
