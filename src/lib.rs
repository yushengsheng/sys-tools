pub mod apps;
pub mod cleanup;
pub mod disk;
pub mod migration;
pub mod paths;
pub mod platform;
pub mod residue;

pub type Result<T> = std::result::Result<T, String>;

pub fn format_size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index < units.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    format!("{value:.1} {}", units[index])
}

