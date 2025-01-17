type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    println!("\n🧹 clean up test artifacts from previous run");
    cargo::clean_test_app()?;

    println!("\n⏳ install latest flip-link");
    cargo::install_flip_link()?;

    println!("\n🧪 cargo test");
    cargo::test()?;

    // ---
    Ok(())
}

mod cargo {
    use super::Result;
    use std::process::Command;

    pub fn clean_test_app() -> Result<()> {
        let status = Command::new("cargo")
            .arg("clean")
            .current_dir("test-flip-link-app")
            .status()?;
        match status.success() {
            false => Err(format!("cleaning `test-flip-link-app`").into()),
            true => Ok(()),
        }
    }

    /// Install local revision of `flip-link`.
    pub fn install_flip_link() -> Result<()> {
        let status = Command::new("cargo")
            .args(&["install", "--debug", "--force", "--path", "."])
            .status()?;
        match status.success() {
            false => Err(format!("installing flip-link from path").into()),
            true => Ok(()),
        }
    }

    pub fn test() -> Result<()> {
        let status = Command::new("cargo")
            // `--test-threads=1` prevents race conditions accessing the elf-file
            .args(&["test", "--", "--test-threads=1"])
            .status()?;
        match status.success() {
            false => Err(format!("running `cargo test`").into()),
            true => Ok(()),
        }
    }
}
