mod argument_parser;
mod linking;

use std::{
    borrow::Cow,
    env,
    fs::{self, File},
    io::Write,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    process,
};

use object::{elf, Object as _, ObjectSection, SectionFlags};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const EXIT_CODE_FAILURE: i32 = 1;
/// Stack Pointer alignment required by the ARM architecture
const SP_ALIGN: u64 = 8;

fn main() -> Result<()> {
    // clippy complains about redundant closure, but in fact
    // the function signature isn't 100% compatible so we must wrap it
    #[allow(clippy::redundant_closure)]
    notmain().map(|code| process::exit(code))
}

fn notmain() -> Result<i32> {
    env_logger::init();

    // NOTE `skip` the name/path of the binary (first argument)
    let args = env::args().skip(1).collect::<Vec<_>>();

    {
        let exit_status = linking::link_normally(&args)?;
        if !exit_status.success() {
            eprintln!();
            eprintln!(
                "flip-link: the native linker failed to link the program normally; \
                 please check your project configuration and linker scripts"
            );
            return Ok(exit_status.code().unwrap_or(EXIT_CODE_FAILURE));
        }
        // if linking succeeds then linker scripts are well-formed; we'll rely on that in the parser
    }

    let current_dir = env::current_dir()?;
    let linker_scripts = get_linker_scripts(&args, &current_dir)?;

    // here we assume that we'll end with the same linker script as LLD
    // I'm unsure about how LLD picks a linker script when there are multiple candidates in the
    // library search path
    let mut ram_path_entry = None;
    for linker_script in linker_scripts {
        let script_contents = fs::read_to_string(linker_script.path())?;

        let mut parser = Parser {
            script: &script_contents,
            position: 0,
            ram_line: 0,
        };

        if let Some(entry) = parser.find_ram_in_linker_script() {
            log::debug!("found {:?} in {}", entry, linker_script.path().display());
            ram_path_entry = Some((linker_script, entry));
            break;
        }
    }
    let (ram_linker_script, ram_entry) = ram_path_entry
        .ok_or_else(|| "MEMORY.RAM not found after scanning linker scripts".to_string())?;

    let output_path = argument_parser::get_output_path(&args)?;
    let elf = &*fs::read(output_path)?;
    let object = object::File::parse(elf)?;

    // TODO assert that `_stack_start == ORIGIN(RAM) + LENGTH(RAM)`
    // if that's not the case the user has specified a custom location for the stack; we should
    // error in that case (e.g. the stack may have been placed in CCRAM)

    // compute the span of RAM sections
    let (used_ram_length, used_ram_align) = compute_span_of_ram_sections(ram_entry, object);

    // the idea is to push `used_ram` all the way to the end of the RAM region
    // to do this we'll use a fake ORIGIN and LENGTH for the RAM region
    // this fake RAM region will be at the end of real RAM region
    let new_origin = round_down_to_nearest_multiple(
        ram_entry.end() - used_ram_length,
        used_ram_align.max(SP_ALIGN),
    );
    let new_length = ram_entry.end() - new_origin;

    log::info!(
        "new RAM region: ORIGIN={:#x}, LENGTH={}",
        new_origin,
        new_length
    );

    // to overwrite RAM we'll create a new linker script in a temporary directory
    let exit_status = in_tempdir(|tempdir| {
        let original_linker_script = fs::read_to_string(ram_linker_script.path())?;
        // XXX in theory could collide with a user-specified linker script
        let mut new_linker_script = File::create(tempdir.join(ram_linker_script.file_name()))?;

        for (index, line) in original_linker_script.lines().enumerate() {
            if index == ram_entry.line {
                writeln!(
                    new_linker_script,
                    "  RAM : ORIGIN = {:#x}, LENGTH = {}",
                    new_origin, new_length
                )?;
            } else {
                writeln!(new_linker_script, "{}", line)?;
            }
        }
        new_linker_script.flush()?;

        let exit_status = linking::link_modified(&args, &current_dir, tempdir, new_origin)?;
        Ok(exit_status)
    })?;

    if !exit_status.success() {
        return Ok(exit_status.code().unwrap_or(EXIT_CODE_FAILURE));
    }

    Ok(0)
}

fn in_tempdir<T>(callback: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    // We avoid the `tempfile` crate because it pulls in quite a few dependencies.

    let mut random = [0; 8];
    getrandom::getrandom(&mut random).map_err(|e| e.to_string())?;

    let mut path = std::env::temp_dir();
    path.push(format!("flip-link-{:02x?}", random));
    fs::create_dir(&path)?;

    let res = callback(&path);

    // Just in case https://github.com/rust-lang/rust/issues/29497 hits us, we ignore the error from
    // removing the directory and just let it linger until the machine reboots ¯\_(ツ)_/¯.
    let _ = fs::remove_dir_all(&path);

    res
}

/// Returns `(used_ram_length, used_ram_align)`
fn compute_span_of_ram_sections(ram_entry: MemoryEntry, object: object::File) -> (u64, u64) {
    let mut used_ram_start = u64::MAX;
    let mut used_ram_end = 0;
    let mut used_ram_align = 0;
    let ram_region_span = ram_entry.span();
    let mut found_a_section = false;
    for section in object.sections() {
        if let SectionFlags::Elf { sh_flags } = section.flags() {
            if sh_flags & elf::SHF_ALLOC as u64 != 0 {
                let start = section.address();
                let size = section.size();
                let end = start + size;

                if ram_region_span.contains(&start) && ram_region_span.contains(&end) {
                    found_a_section = true;
                    log::debug!(
                        "{} resides in RAM",
                        section.name().unwrap_or("nameless section")
                    );
                    used_ram_align = used_ram_align.max(section.align());

                    if used_ram_start > start {
                        used_ram_start = start;
                    }

                    if used_ram_end < end {
                        used_ram_end = end;
                    }
                }
            }
        }
    }

    let used_ram_length = if !found_a_section {
        used_ram_start = ram_entry.origin;
        0
    } else {
        used_ram_end - used_ram_start
    };

    log::info!(
        "used RAM spans: origin={:#x}, length={}, align={}",
        used_ram_start,
        used_ram_length,
        used_ram_align
    );

    (used_ram_length, used_ram_align)
}

fn round_down_to_nearest_multiple(x: u64, multiple: u64) -> u64 {
    x - (x % multiple)
}

struct LinkerScript(PathBuf);

impl LinkerScript {
    fn new(path: PathBuf) -> Self {
        assert!(path.is_file());
        Self(path)
    }

    fn file_name(&self) -> &str {
        self.path().file_name().unwrap().to_str().unwrap()
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

fn get_linker_scripts(args: &[String], current_dir: &Path) -> Result<Vec<LinkerScript>> {
    let mut search_paths = vec![current_dir.into()];
    search_paths.extend(argument_parser::get_search_paths(args));

    let mut search_targets = argument_parser::get_search_targets(args);

    // try to find all linker scripts from `search_list` in the `search_paths`
    let mut linker_scripts = vec![];
    while let Some(filename) = search_targets.pop() {
        for dir in &search_paths {
            let full_path = dir.join(&*filename);

            if full_path.exists() {
                log::trace!("found {} in {}", filename, dir.display());
                let contents = fs::read_to_string(&full_path)?;

                // also load linker scripts `INCLUDE`d by other scripts
                for include in get_includes_from_linker_script(&contents) {
                    log::trace!("{} INCLUDEs {}", filename, include);
                    search_targets.push(Cow::Owned(include.to_string()));
                }

                linker_scripts.push(LinkerScript::new(full_path));
                break;
            }
        }
    }

    Ok(linker_scripts)
}

/// Entry under the `MEMORY` section in a linker script
#[derive(Clone, Copy, Debug, PartialEq)]
struct MemoryEntry {
    line: usize,
    origin: u64,
    length: u64,
}

impl MemoryEntry {
    fn end(&self) -> u64 {
        self.origin + self.length
    }

    fn span(&self) -> RangeInclusive<u64> {
        self.origin..=self.end()
    }
}

/// Rm `token` from beginning of `line`, else `continue` loop iteration
macro_rules! eat {
    ($line:expr, $token:expr) => {
        if let Some(a) = $line.strip_prefix($token) {
            a.trim()
        } else {
            continue;
        }
    };
}

fn get_includes_from_linker_script(linker_script: &str) -> Vec<&str> {
    let mut includes = vec![];
    for mut line in linker_script.lines() {
        line = line.trim();
        line = eat!(line, "INCLUDE");
        includes.push(line);
    }

    includes
}

struct Parser<'a> {
    script: &'a str,
    // this is to track line number to write back the original linker_script
    ram_line: usize,
    position: usize,
}

impl Parser<'_> {
    /// Looks for "RAM : ORIGIN = $origin, LENGTH = $length"
    fn find_ram_in_linker_script(&mut self) -> Option<MemoryEntry> {
        log::debug!("Find RAM: {}", &self.script);

        // finds at which line RAM is
        self.get_ram_line();

        if let Some(pos) = self.script.find("RAM") {
            self.advance(pos);
        } else {
            return None;
        }

        self.parse_token("RAM");
        log::debug!("Parse RAM: {}", &self.script);
        // jump over attributes like (xrw) see parse_attributes()
        if let Some(i) = self.script.find(':') {
            self.advance(i);
        }

        self.parse_token(":");
        self.parse_token("ORIGIN");
        self.parse_token("=");
        log::debug!("Parse ORIGN = : {}", &self.script);
        let origin = self.parse_expression();

        self.parse_token(",");
        self.parse_token("LENGTH");
        self.parse_token("=");
        log::debug!("Parse Length = : {}", &self.script);
        let total_length = self.parse_expression();

        log::debug!(
            "Memory Entry. \n RAM_line: {} ORIGIN: {}, length: {} ",
            &self.ram_line,
            origin,
            total_length
        );
        Some(MemoryEntry {
            line: self.ram_line,
            origin,
            length: total_length,
        })
    }

    /// This function looks for the RAM position in the original script.
    fn get_ram_line(&mut self) {
        for (index, line) in self.script.lines().enumerate() {
            if line.contains("RAM") {
                self.ram_line = index;
                return;
            } else {
                self.ram_line = 0;
            }
        }
    }
    fn parse_token(&mut self, expect: &str) {
        self.eat_whitespace();

        if self.script.starts_with(expect) {
            self.advance(expect.len());
        }
    }

    fn eat_whitespace(&mut self) {
        while let Some(ch) = self.script.chars().next() {
            if ch == ' ' || ch == '\t' || ch == '\r' || ch == '\n' {
                self.advance(1);
            } else {
                return;
            }
        }
    }

    fn advance(&mut self, amount: usize) {
        self.script = &self.script[amount..];
        self.position += amount;
    }

    fn parse_expression(&mut self) -> u64 {
        let mut total = 0;

        // we assume that there can be many additions in a row...
        loop {
            total += self.parse_number();

            self.eat_whitespace();

            if self.script.starts_with('+') {
                self.advance(1);
            } else {
                return total;
            }
        }
    }

    fn parse_number(&mut self) -> u64 {
        log::debug!("Number in parse number:{}", &self.script);
        self.eat_whitespace();

        // find if the number is base 10 or 16
        let radix = if self.script.starts_with("0x") {
            self.advance("0x".len());
            16
        } else {
            10
        };
        // we need to know how long the number is
        // that returns the position of the first non-hex-digit char in the input
        let number_len = self
            .script
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(self.script.len());

        let mut number = u64::from_str_radix(&self.script[..number_len], radix).unwrap();
        self.advance(number_len);
        match self.script.chars().next() {
            Some('K') => {
                number *= 1024;
                self.advance(1);
            }
            Some('k') => {
                number *= 1024;
                self.advance(1);
            }
            Some('M') => {
                number *= 1024 * 1024;
                self.advance(1);
            }
            Some('m') => {
                number *= 1024 * 1024;
                self.advance(1);
            }
            _ => {}
        }

        number
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        const LINKER_SCRIPT: &str = "MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 256K
  RAM : ORIGIN = 0x20000000, LENGTH = 64K
}

INCLUDE device.x
";
        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };

        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20000000,
                length: 64 * 1024,
            })
        );

        assert_eq!(
            get_includes_from_linker_script(LINKER_SCRIPT),
            vec!["device.x"]
        );
    }

    // Should return error.
    //     #[test]
    //     fn parse_unauthorized_units() {
    //         const LINKER_SCRIPT: &str = "MEMORY
    // {
    //     FLASH : ORIGIN = 0x00000000, LENGTH = 256P
    //     RAM : ORIGIN = 0x20000000, LENGTH = 64P
    // }
    // ";
    //         let mut parser = Parser {
    //             script: LINKER_SCRIPT,
    //             position: 0,
    //             ram_line: 0,
    //         };
    //         assert_eq!(parser.find_ram_in_linker_script(), None);
    //     }
    #[test]
    fn parse_no_units() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
      FLASH : ORIGIN = 0x00000000, LENGTH = 262144
      RAM : ORIGIN = 0x20000000, LENGTH = 65536
    }

    INCLUDE device.x
    ";
        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };

        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20000000,
                length: 64 * 1024,
            })
        );

        assert_eq!(
            get_includes_from_linker_script(LINKER_SCRIPT),
            vec!["device.x"]
        );
    }

    #[test]
    fn parse_plus() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
      FLASH : ORIGIN = 0x08000000, LENGTH = 2M
      RAM : ORIGIN = 0x20020000, LENGTH = 368K + 16K
    }

    INCLUDE device.x
    ";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };

        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20020000,
                length: (368 + 16) * 1024,
            })
        );

        assert_eq!(
            get_includes_from_linker_script(LINKER_SCRIPT),
            vec!["device.x"]
        );
    }

    //
    #[test]
    fn parse_plus_origin_k() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
      FLASH : ORIGIN = 0x08000000, LENGTH = 2M
      RAM : ORIGIN = 0x20020000 + 100K, LENGTH = 368K
    }
    ";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };

        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20020000 + (100 * 1024),
                length: 368 * 1024,
            })
        );
    }

    #[test]
    fn parse_plus_origin_m() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
      FLASH : ORIGIN = 0x08000000, LENGTH = 2M
      RAM : ORIGIN = 0x20020000 + 100M, LENGTH = 368K
    }

    INCLUDE device.x
    ";
        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };
        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20020000 + (100 * 1024 * 1024),
                length: 368 * 1024,
            })
        );

        assert_eq!(
            get_includes_from_linker_script(LINKER_SCRIPT),
            vec!["device.x"]
        );
    }

    // test attributes https://sourceware.org/binutils/docs/ld/MEMORY.html
    #[test]
    fn parse_attributes() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
        /* NOTE 1 K = 1 KiBi = 1024 bytes */
        FLASH (rx) : ORIGIN = 0x08000000, LENGTH = 1024K
        RAM (xrw)  : ORIGIN = 0x20000000, LENGTH = 128K
    }
    ";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };
        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 4,
                origin: 0x20000000,
                length: 128 * 1024,
            })
        );
    }

    //     // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_new_line() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
        FLASH : ORIGIN = 0x08000000, LENGTH = 2M
        RAM : ORIGIN = 0x20000000,
        LENGTH = 256K
    }
    ";
        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };
        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20000000,
                length: 256 * 1024,
            })
        );
    }

    // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_new_lines() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
      RAM
      :
      ORIGIN
      =
      0x20000000
      ,
    LENGTH
    =
    256K
    }
    ";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };
        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 2,
                origin: 0x20000000,
                length: 256 * 1024,
            })
        );
    }

    //     // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_same_line() {
        const LINKER_SCRIPT: &str =  "MEMORY { FLASH : ORIGIN = 0x00000000, LENGTH = 1024K RAM : ORIGIN = 0x20000000, LENGTH = 256K }";
        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };
        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 0,
                origin: 0x20000000,
                length: 256 * 1024,
            })
        );
    }

    //     // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_section() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
        FLASH : ORIGIN = 0x00000000, LENGTH = 1024K
        RAM : ORIGIN = 0x20000000, LENGTH = 256K
    };

    SECTIONS {};";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };

        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20000000,
                length: 256 * 1024,
            })
        );
    }

    //     // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_memory_replicate() {
        const LINKER_SCRIPT: &str = "
    MEMORY { FLASH : ORIGIN = 0x00000000, LENGTH = 1024K }
    MEMORY { RAM : ORIGIN = 0x20000000, LENGTH = 256K }";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };

        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 2,
                origin: 0x20000000,
                length: 256 * 1024,
            })
        );
    }

    //     // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_sections_replicate() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
        FLASH : ORIGIN = 0x08000000, LENGTH = 2M
        RAM : ORIGIN = 0x20020000, LENGTH = 368K
    }
    SECTIONS {}
    SECTIONS {}";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };

        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20020000,
                length: 368 * 1024,
            })
        );
    }

    //     // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_sections_include_memory() {
        const LINKER_SCRIPT: &str = "MEMORY
    {
        FLASH : ORIGIN = 0x08000000, LENGTH = 2M
        RAM : ORIGIN = 0x20020000, LENGTH = 368K
    }

    SECTIONS {
    MEMORY : {}
    }";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };
        assert_eq!(
            parser.find_ram_in_linker_script(),
            Some(MemoryEntry {
                line: 3,
                origin: 0x20020000,
                length: 368 * 1024,
            })
        );
    }

    //     // Flip link should accept. Accepted by rust lld
    #[test]
    fn parse_comment() {
        const LINKER_SCRIPT: &str = "/*
    MEMORY
    {
        RAM (rw) : ORIGIN = 0x20020000, LENGTH = 368K
    }
    */
    ";

        let mut parser = Parser {
            script: LINKER_SCRIPT,
            position: 0,
            ram_line: 0,
        };
        assert_eq!(parser.find_ram_in_linker_script(), None);
    }
}

// // Flip link should accept. Accepted by rust lld

#[test]
fn parse_empty() {
    const LINKER_SCRIPT: &str = "
    MEMORY {}

    SECTIONS {
    MEMORY : {}
    }
    ";
    let mut parser = Parser {
        script: LINKER_SCRIPT,
        position: 0,
        ram_line: 0,
    };
    assert_eq!(parser.find_ram_in_linker_script(), None);
}
