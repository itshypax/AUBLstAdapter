//! Finds game folder and config on its own, so a new player only presses "Code anfordern".

use std::fs;
use std::path::{Path, PathBuf};

use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

const INSTALL_DIRS: [&str; 2] = [
    r"C:\Program Files\sixteentons entertainment\Emergency4",
    r"C:\Program Files (x86)\sixteentons entertainment\Emergency4",
];

pub fn is_game_dir(dir: &Path) -> bool {
    dir.join("Em4.exe").is_file()
}

/// A mod ships the adapter inside the game folder, so the exe's own parents come first,
/// then every Steam library, then the retail install.
pub fn find_game_dir(exe: &Path) -> Option<PathBuf> {
    let parents = exe.ancestors().skip(1).map(Path::to_path_buf);
    let steam = steam_libraries().into_iter().flat_map(|lib| subdirs(&lib.join(r"steamapps\common")));
    parents.chain(steam).chain(INSTALL_DIRS.map(PathBuf::from)).find(|dir| is_game_dir(dir))
}

/// A config beside the exe wins, as a mod ships it. Otherwise one in the mod folders:
/// the game starts a mod by its folder name, so only a config whose mod id names its
/// own mod folder fits. Of those the highest; two at the same height are a guess, so
/// the player picks. `mod_id` reads a config, None when it isn't one.
pub fn find_config(exe: &Path, game_dir: Option<&Path>, mod_id: impl Fn(&Path) -> Option<String>) -> Option<PathBuf> {
    let mut found = Vec::new();
    if let Some(dir) = exe.parent() {
        cfg_files(dir, 0, 0, &mut found);
    }
    found.retain(|(_, p)| mod_id(p).is_some());
    if found.is_empty()
        && let Some(game) = game_dir
    {
        let mods = game.join("Mods");
        // Mods\<mod>\<folder>\<config> at most: one level deeper means thousands of
        // model and texture folders, and a window that takes seconds to open.
        cfg_files(&mods, 0, 2, &mut found);
        found.retain(|(_, p)| {
            let folder = p.strip_prefix(&mods).ok().and_then(|rel| rel.components().next());
            let folder = folder.map(|c| c.as_os_str().to_string_lossy().into_owned());
            mod_id(p).zip(folder).is_some_and(|(id, folder)| id.eq_ignore_ascii_case(&folder))
        });
    }
    found.sort();
    match found.as_slice() {
        [(depth, path), rest @ ..] if rest.first().is_none_or(|(d, _)| d > depth) => Some(path.clone()),
        _ => None,
    }
}

fn cfg_files(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<(usize, PathBuf)>) {
    for path in subdirs_and_files(dir) {
        if path.is_dir() {
            if depth < max_depth {
                cfg_files(&path, depth + 1, max_depth, out);
            }
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cfg")) {
            out.push((depth, path));
        }
    }
}

fn subdirs_and_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map(|entries| entries.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    subdirs_and_files(dir).into_iter().filter(|p| p.is_dir()).collect()
}

fn steam_libraries() -> Vec<PathBuf> {
    let steam = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Valve\Steam")
        .and_then(|k| k.get_value::<String, _>("SteamPath"));
    let Ok(steam) = steam else { return Vec::new() };
    fs::read_to_string(Path::new(&steam).join(r"steamapps\libraryfolders.vdf"))
        .map(|text| library_paths(&text))
        .unwrap_or_default()
}

/// The `"path"` lines of Steam's libraryfolders.vdf, backslashes unescaped.
fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| line.trim().strip_prefix("\"path\""))
        .map(|value| PathBuf::from(value.trim().trim_matches('"').replace(r"\\", r"\")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_libraries_come_from_the_path_lines() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"G:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(library_paths(vdf), [PathBuf::from(r"C:\Program Files (x86)\Steam"), PathBuf::from(r"G:\SteamLibrary")]);
    }

    #[test]
    fn the_game_and_its_config_are_found_from_the_mod_folder() {
        let root = std::env::temp_dir().join("aublst-adapter-setup-test");
        let _ = fs::remove_dir_all(&root);
        let lst = root.join(r"Game\Mods\AUBMP\4_LST");
        fs::create_dir_all(lst.join("DE")).unwrap();
        fs::write(root.join(r"Game\Em4.exe"), "").unwrap();
        fs::write(lst.join("AUBMP.cfg"), "AUBMP").unwrap();
        fs::write(lst.join(r"DE\AUBMP.cfg"), "AUBMP").unwrap();
        let mod_id = |p: &Path| fs::read_to_string(p).ok().filter(|t| !t.is_empty());

        let exe = lst.join("AUBLstAdapter.exe");
        assert_eq!(find_game_dir(&exe), Some(root.join("Game")));
        assert_eq!(find_config(&exe, None, mod_id), Some(lst.join("AUBMP.cfg")));

        // Started from elsewhere: the config in Mods wins, the language copies sit too
        // deep, and a copied mod whose config still names AUBMP doesn't count.
        let copy = root.join(r"Game\Mods\AUBMP Edited\4_LST");
        fs::create_dir_all(&copy).unwrap();
        fs::write(copy.join("AUBMP.cfg"), "AUBMP").unwrap();
        let elsewhere = root.join(r"Downloads\AUBLstAdapter.exe");
        assert_eq!(find_config(&elsewhere, Some(&root.join("Game")), mod_id), Some(lst.join("AUBMP.cfg")));

        // Two fitting mods at the same height: no guess.
        fs::create_dir_all(root.join(r"Game\Mods\Other\4_LST")).unwrap();
        fs::write(root.join(r"Game\Mods\Other\4_LST\Other.cfg"), "Other").unwrap();
        assert_eq!(find_config(&elsewhere, Some(&root.join("Game")), mod_id), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
