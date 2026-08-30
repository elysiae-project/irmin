mod hk4e;
mod hkrpg;
mod nap;

pub use hk4e::{
    filter_hk4e_asset_list, find_hk4e_persistent_dir,
    write_audio_lang_record as write_hk4e_audio_lang_record, write_pkg_version_from_manifest,
};
pub use hkrpg::{
    filter_hkrpg_asset_list, write_app_info as write_hkrpg_app_info,
    write_audio_lang_record as write_hkrpg_audio_lang_record,
    write_binary_version_files as write_hkrpg_binary_version_files,
};
pub use nap::{filter_nap_asset_list, filter_nap_installers, write_nap_audio_lang_records};

/// True when `target_file_path` is a CDN-shipped file that sophon regenerates
/// locally (filtered or rewritten), so a byte-level CDN patch for it can never
/// apply and must be skipped during preinstall apply.
pub fn is_sophon_synthesized_asset(game_code: &str, target_file_path: &str) -> bool {
    let name = target_file_path.rsplit('/').next().unwrap_or(target_file_path);
    match game_code {
        "hk4e" => {
            name == "pkg_version"
                || name == "beyond_pkg_version"
                || name.starts_with("Audio_") && name.ends_with("_pkg_version")
                || name.to_lowercase().ends_with("ctable_streaming.dat")
        }
        "hkrpg" => name == "app.info",
        _ => false,
    }
}

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

pub(crate) fn write_lang_file(
    path: &Path,
    vo_langs: &[String],
    mapper: fn(&str) -> Option<&'static str>,
) -> std::io::Result<()> {
    let mut existing: Vec<String> = Vec::new();
    if path.exists()
        && let Ok(content) = fs::read_to_string(path)
    {
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                existing.push(trimmed.to_string());
            }
        }
    }

    for lang in vo_langs {
        if let Some(name) = mapper(lang)
            && !existing.iter().any(|e| e == name)
        {
            existing.push(name.to_string());
        }
    }

    let mut content = String::new();
    for name in &existing {
        content.push_str(name);
        content.push('\n');
    }

    let mut file = File::create(path)?;
    file.write_all(content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // is_sophon_synthesized_asset
    #[test]
    fn test_is_sophon_synthesized_asset_hk4e_pkg_version() {
        assert!(is_sophon_synthesized_asset("hk4e", "pkg_version"));
        assert!(is_sophon_synthesized_asset("hk4e", "beyond_pkg_version"));
        assert!(is_sophon_synthesized_asset(
            "hk4e",
            "Audio_English(US)_pkg_version"
        ));
        assert!(is_sophon_synthesized_asset(
            "hk4e",
            "data/Audio_Japanese_pkg_version"
        ));
    }

    #[test]
    fn test_is_sophon_synthesized_asset_hk4e_ctable() {
        assert!(is_sophon_synthesized_asset(
            "hk4e",
            "GenshinImpact_Data/StreamingAssets/ctable_streaming.dat"
        ));
        assert!(is_sophon_synthesized_asset(
            "hk4e",
            "GenshinImpact_Data/StreamingAssets/CTABLE_STREAMING.DAT"
        ));
        assert!(!is_sophon_synthesized_asset(
            "hk4e",
            "GenshinImpact_Data/StreamingAssets/normal.dat"
        ));
    }

    #[test]
    fn test_is_sophon_synthesized_asset_hkrpg_app_info() {
        assert!(is_sophon_synthesized_asset(
            "hkrpg",
            "StarRail_Data/app.info"
        ));
        assert!(!is_sophon_synthesized_asset(
            "hkrpg",
            "StarRail_Data/StreamingAssets/BinaryVersion.bytes"
        ));
    }

    #[test]
    fn test_is_sophon_synthesized_asset_other_games_false() {
        assert!(!is_sophon_synthesized_asset("nap", "pkg_version"));
        assert!(!is_sophon_synthesized_asset("bh3", "ZenlessZoneZero_Data/app.info"));
        assert!(!is_sophon_synthesized_asset("np", "GenshinImpact.exe"));
    }

    #[test]
    fn test_is_sophon_synthesized_asset_regular_files_false() {
        assert!(!is_sophon_synthesized_asset("hk4e", "GenshinImpact.exe"));
        assert!(!is_sophon_synthesized_asset("hk4e", "pkg_version.txt"));
        assert!(!is_sophon_synthesized_asset("hk4e", ""));
    }

    // write_lang_file
    #[test]
    fn test_write_lang_file_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lang_file.txt");

        let vo_langs = vec!["en-us".to_string(), "ja-jp".to_string()];

        write_lang_file(&path, &vo_langs, |locale| match locale {
            "en-us" => Some("English(US)"),
            "ja-jp" => Some("Japanese"),
            _ => None,
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "English(US)\nJapanese\n");
    }

    #[test]
    fn test_write_lang_file_append_to_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lang_file.txt");
        fs::write(&path, "Chinese\n").unwrap();

        let vo_langs = vec!["en-us".to_string(), "ja-jp".to_string()];

        write_lang_file(&path, &vo_langs, |locale| match locale {
            "en-us" => Some("English(US)"),
            "ja-jp" => Some("Japanese"),
            _ => None,
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "Chinese\nEnglish(US)\nJapanese\n");
    }

    #[test]
    fn test_write_lang_file_does_not_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lang_file.txt");
        fs::write(&path, "English(US)\n").unwrap();

        let vo_langs = vec!["en-us".to_string()];

        write_lang_file(&path, &vo_langs, |locale| match locale {
            "en-us" => Some("English(US)"),
            _ => None,
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "English(US)\n");
    }

    #[test]
    fn test_write_lang_file_empty_vo_langs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lang_file.txt");

        write_lang_file(&path, &[], |_| -> Option<&'static str> { None }).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_write_lang_file_skips_none_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lang_file.txt");

        let vo_langs = vec![
            "en-us".to_string(),
            "unknown".to_string(),
            "ja-jp".to_string(),
        ];

        write_lang_file(&path, &vo_langs, |locale| match locale {
            "en-us" => Some("English(US)"),
            "ja-jp" => Some("Japanese"),
            "unknown" => None,
            _ => None,
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "English(US)\nJapanese\n");
    }

    #[test]
    fn test_write_lang_file_mapper_en_us() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lang_file.txt");

        let vo_langs = vec!["en-us".to_string()];

        write_lang_file(&path, &vo_langs, |locale| match locale {
            "en-us" => Some("English(US)"),
            _ => None,
        })
        .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "English(US)\n");
    }
}
