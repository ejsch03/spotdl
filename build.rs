fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // the desktop app's UI; compiled into Rust code that `spotdl-gui` includes
    #[cfg(feature = "gui")]
    {
        let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
        slint_build::compile_with_config("ui/app.slint", config)
            .expect("the Slint UI should compile");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows::embed_resources();
    }
}

/// The icon and version details Windows shows for spotdl.exe and spotdl-gui.exe (in
/// Explorer, the taskbar, Task Manager and Properties). The resource file is written here
/// rather than with a resource compiler, so building needs no extra tools.
mod windows {
    use std::path::{Path, PathBuf};

    const ICON: &str = "assets/icon/spotdl.ico";

    // resource types, and the flags resource compilers give each
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    const RT_VERSION: u16 = 16;
    const MOVEABLE_DISCARDABLE: u16 = 0x1010;
    const MOVEABLE_PURE_DISCARDABLE: u16 = 0x1030;
    const MOVEABLE_PURE: u16 = 0x0030;
    const LANG_NEUTRAL: u16 = 0;
    const LANG_EN_US: u16 = 0x0409;

    pub fn embed_resources() {
        println!("cargo:rerun-if-changed={ICON}");
        let icon = std::fs::read(ICON).expect("the icon should be readable");
        let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
        let res = out.join("spotdl.res");
        std::fs::write(&res, resources(&icon)).expect("OUT_DIR should be writable");

        match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            // Microsoft's linker (and lld-link) take resource files as they are
            Ok("msvc") => println!("cargo:rustc-link-arg-bins={}", res.display()),
            // GNU's linker wants an object file, which windres makes
            Ok("gnu") => match to_object(&res, &out) {
                Some(object) => println!("cargo:rustc-link-arg-bins={}", object.display()),
                None => println!(
                    "cargo:warning=windres wasn't found, so the programs are built without \
                     their icon"
                ),
            },
            _ => {}
        }
    }

    fn to_object(res: &Path, out: &Path) -> Option<PathBuf> {
        let object = out.join("spotdl-resources.o");
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let candidates = [
            std::env::var("WINDRES").ok(),
            Some(format!("{arch}-w64-mingw32-windres")),
            Some("windres".into()),
        ];
        candidates.into_iter().flatten().find_map(|windres| {
            let status = std::process::Command::new(windres)
                .arg("--input")
                .arg(res)
                .args(["--input-format=res", "--output-format=coff", "--output"])
                .arg(&object)
                .status()
                .ok()?;
            status.success().then(|| object.clone())
        })
    }

    /// A Win32 resource (.res) file: the icon's images, the icon group pointing at them
    /// (resource 1, the one Windows shows for the program), and version details.
    fn resources(ico: &[u8]) -> Vec<u8> {
        // every .res file opens with an empty entry
        let mut res = entry(0, 0, 0, 0, &[]);
        let images = icon_images(ico);
        let mut group = Vec::new();
        group.extend(0u16.to_le_bytes()); // reserved
        group.extend(1u16.to_le_bytes()); // an icon (not a cursor)
        group.extend((images.len() as u16).to_le_bytes());
        for (i, image) in images.iter().enumerate() {
            let id = i as u16 + 1;
            res.extend(entry(
                RT_ICON,
                id,
                MOVEABLE_DISCARDABLE,
                LANG_NEUTRAL,
                image.data,
            ));
            // the directory entry from the .ico, with the resource id for its offset
            group.extend(&image.header[..12]);
            group.extend(id.to_le_bytes());
        }
        res.extend(entry(
            RT_GROUP_ICON,
            1,
            MOVEABLE_PURE_DISCARDABLE,
            LANG_NEUTRAL,
            &group,
        ));
        res.extend(entry(
            RT_VERSION,
            1,
            MOVEABLE_PURE,
            LANG_EN_US,
            &version_info(),
        ));
        res
    }

    /// One resource, with a numbered type and name, padded to a 4-byte boundary.
    fn entry(kind: u16, name: u16, flags: u16, language: u16, data: &[u8]) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend((data.len() as u32).to_le_bytes());
        e.extend(32u32.to_le_bytes()); // this header's size
        e.extend(
            [0xFFFF, kind, 0xFFFF, name]
                .iter()
                .flat_map(|w: &u16| w.to_le_bytes()),
        );
        e.extend(0u32.to_le_bytes()); // data version
        e.extend(flags.to_le_bytes());
        e.extend(language.to_le_bytes());
        e.extend(0u32.to_le_bytes()); // version
        e.extend(0u32.to_le_bytes()); // characteristics
        e.extend(data);
        pad(&mut e);
        e
    }

    struct IconImage<'a> {
        /// The 16-byte directory entry from the .ico file.
        header: &'a [u8],
        data: &'a [u8],
    }

    fn icon_images(ico: &[u8]) -> Vec<IconImage<'_>> {
        let word = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]) as usize;
        let dword =
            |at: usize| u32::from_le_bytes(ico[at..at + 4].try_into().expect("4 bytes")) as usize;
        assert!(word(0) == 0 && word(2) == 1, "{ICON} isn't an icon file");
        (0..word(4))
            .map(|i| {
                let header = &ico[6 + 16 * i..6 + 16 * (i + 1)];
                let (size, offset) = (dword(6 + 16 * i + 8), dword(6 + 16 * i + 12));
                IconImage {
                    header,
                    data: &ico[offset..offset + size],
                }
            })
            .collect()
    }

    /// VS_VERSIONINFO, with the version from Cargo.toml.
    fn version_info() -> Vec<u8> {
        let part = |name| -> u32 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        let (major, minor, patch) = (
            part("CARGO_PKG_VERSION_MAJOR"),
            part("CARGO_PKG_VERSION_MINOR"),
            part("CARGO_PKG_VERSION_PATCH"),
        );
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

        let mut fixed = Vec::new();
        for dword in [
            0xFEEF_04BD,           // signature
            0x0001_0000,           // structure version
            (major << 16) | minor, // file version
            patch << 16,           //
            (major << 16) | minor, // product version
            patch << 16,           //
            0x3F,                  // which flags are meaningful
            0,                     // flags: a release build
            0x0004_0004,           // for 32-bit (and later) Windows NT
            1,                     // an application
            0,                     // subtype
            0,                     // date
            0,                     //
        ] {
            fixed.extend(u32::to_le_bytes(dword));
        }

        let strings: Vec<Vec<u8>> = [
            ("CompanyName", "spotdl"),
            ("FileDescription", "spotdl"),
            ("FileVersion", &version),
            ("InternalName", "spotdl"),
            ("ProductName", "spotdl"),
            ("ProductVersion", &version),
        ]
        .into_iter()
        .map(|(key, value)| block(key, Value::Text(value), &[]))
        .collect();
        let table = block("040904B0", Value::None, &strings); // US English, Unicode
        let string_info = block("StringFileInfo", Value::None, &[table]);
        let translation = block("Translation", Value::Binary(&[0x09, 0x04, 0xB0, 0x04]), &[]);
        let var_info = block("VarFileInfo", Value::None, &[translation]);
        block(
            "VS_VERSION_INFO",
            Value::Binary(&fixed),
            &[string_info, var_info],
        )
    }

    enum Value<'a> {
        None,
        Text(&'a str),
        Binary(&'a [u8]),
    }

    /// A block of version information: its length, its value's length and kind, its key,
    /// its value, then its children, each part starting on a 4-byte boundary. The length
    /// leaves out padding after the last part.
    fn block(key: &str, value: Value, children: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0; 6];
        b.extend(key.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        pad(&mut b);
        let (value_length, kind) = match value {
            Value::None => (0, 1),
            Value::Text(text) => {
                let words: Vec<u16> = text.encode_utf16().chain([0]).collect();
                b.extend(words.iter().flat_map(|w| w.to_le_bytes()));
                (words.len(), 1) // counted in characters
            }
            Value::Binary(bytes) => {
                b.extend(bytes);
                (bytes.len(), 0) // counted in bytes
            }
        };
        for child in children {
            pad(&mut b);
            b.extend(child);
        }
        let length = b.len() as u16;
        b[0..2].copy_from_slice(&length.to_le_bytes());
        b[2..4].copy_from_slice(&(value_length as u16).to_le_bytes());
        b[4..6].copy_from_slice(&(kind as u16).to_le_bytes());
        b
    }

    fn pad(bytes: &mut Vec<u8>) {
        bytes.resize(bytes.len().next_multiple_of(4), 0);
    }
}
