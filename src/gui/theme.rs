//! The desktop app's color themes, chosen in Settings and saved in `gui.json`.
//!
//! Themes that come as a light and dark pair (GitHub, Solarized, ...) start in the half
//! that matches the system's light or dark setting as it is when the app starts. Switching
//! the system between light and dark while the app runs leaves the theme alone, and so
//! does picking the other half in Settings, until the next start.

use crate::ThemeColors;

/// What's used until one is chosen, in its light or dark half.
const DEFAULT: &str = "github-light";

pub struct Theme {
    /// As saved in `gui.json`.
    pub id: &'static str,
    /// As listed in Settings.
    pub name: &'static str,
    pub dark: bool,
    /// The other half of a light and dark pair.
    pair: Option<&'static str>,
    colors: Colors,
}

/// The colors the window uses (`ThemeColors` in ui/widgets.slint), as 0xRRGGBB.
struct Colors {
    background: u32,
    /// buttons, text boxes and menus
    control: u32,
    foreground: u32,
    /// secondary text
    muted: u32,
    border: u32,
    /// primary buttons, the selected filter, progress and focus
    accent: u32,
    /// text on the accent color
    accent_foreground: u32,
    good: u32,
    bad: u32,
}

/// In the order Settings lists them.
pub const THEMES: [Theme; 14] = [
    Theme {
        id: "light",
        name: "Light",
        dark: false,
        pair: Some("dark"),
        colors: Colors {
            background: 0xf9f9f9,
            control: 0xffffff,
            foreground: 0x1a1a1a,
            muted: 0x616161,
            border: 0xd1d1d1,
            accent: 0x005fb8,
            accent_foreground: 0xffffff,
            good: 0x0f7b0f,
            bad: 0xc42b1c,
        },
    },
    Theme {
        id: "dark",
        name: "Dark",
        dark: true,
        pair: Some("light"),
        colors: Colors {
            background: 0x1c1c1c,
            control: 0x2d2d2d,
            foreground: 0xffffff,
            muted: 0xa9a9a9,
            border: 0x3d3d3d,
            accent: 0x60cdff,
            accent_foreground: 0x000000,
            good: 0x6ccb5f,
            bad: 0xff99a4,
        },
    },
    Theme {
        id: "github-light",
        name: "GitHub Light",
        dark: false,
        pair: Some("github-dark"),
        colors: Colors {
            background: 0xffffff,
            control: 0xf6f8fa,
            foreground: 0x1f2328,
            muted: 0x59636e,
            border: 0xd1d9e0,
            accent: 0x0969da,
            accent_foreground: 0xffffff,
            good: 0x1a7f37,
            bad: 0xd1242f,
        },
    },
    Theme {
        id: "github-dark",
        name: "GitHub Dark",
        dark: true,
        pair: Some("github-light"),
        colors: Colors {
            background: 0x0d1117,
            control: 0x212830,
            foreground: 0xf0f6fc,
            muted: 0x9198a1,
            border: 0x3d444d,
            accent: 0x1f6feb,
            accent_foreground: 0xffffff,
            good: 0x3fb950,
            bad: 0xf85149,
        },
    },
    Theme {
        id: "nord",
        name: "Nord",
        dark: true,
        pair: None,
        colors: Colors {
            background: 0x2e3440,
            control: 0x3b4252,
            foreground: 0xeceff4,
            muted: 0xa3abb9,
            border: 0x4c566a,
            accent: 0x88c0d0,
            accent_foreground: 0x2e3440,
            good: 0xa3be8c,
            bad: 0xbf616a,
        },
    },
    Theme {
        id: "dracula",
        name: "Dracula",
        dark: true,
        pair: None,
        colors: Colors {
            background: 0x282a36,
            control: 0x343746,
            foreground: 0xf8f8f2,
            muted: 0x8e99c2,
            border: 0x44475a,
            accent: 0xbd93f9,
            accent_foreground: 0x282a36,
            good: 0x50fa7b,
            bad: 0xff5555,
        },
    },
    Theme {
        id: "solarized-light",
        name: "Solarized Light",
        dark: false,
        pair: Some("solarized-dark"),
        colors: Colors {
            background: 0xfdf6e3,
            control: 0xeee8d5,
            foreground: 0x073642,
            muted: 0x586e75,
            border: 0xddd6c1,
            accent: 0x268bd2,
            accent_foreground: 0xfdf6e3,
            good: 0x859900,
            bad: 0xdc322f,
        },
    },
    Theme {
        id: "solarized-dark",
        name: "Solarized Dark",
        dark: true,
        pair: Some("solarized-light"),
        colors: Colors {
            background: 0x002b36,
            control: 0x073642,
            foreground: 0xeee8d5,
            muted: 0x93a1a1,
            border: 0x2c4d56,
            accent: 0x268bd2,
            accent_foreground: 0xfdf6e3,
            good: 0x859900,
            bad: 0xdc322f,
        },
    },
    Theme {
        id: "gruvbox-light",
        name: "Gruvbox Light",
        dark: false,
        pair: Some("gruvbox-dark"),
        colors: Colors {
            background: 0xfbf1c7,
            control: 0xf2e5bc,
            foreground: 0x3c3836,
            muted: 0x7c6f64,
            border: 0xd5c4a1,
            accent: 0xaf3a03,
            accent_foreground: 0xfbf1c7,
            good: 0x79740e,
            bad: 0x9d0006,
        },
    },
    Theme {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        dark: true,
        pair: Some("gruvbox-light"),
        colors: Colors {
            background: 0x282828,
            control: 0x3c3836,
            foreground: 0xebdbb2,
            muted: 0xa89984,
            border: 0x504945,
            accent: 0xfe8019,
            accent_foreground: 0x282828,
            good: 0xb8bb26,
            bad: 0xfb4934,
        },
    },
    Theme {
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        dark: false,
        pair: Some("catppuccin-mocha"),
        colors: Colors {
            background: 0xeff1f5,
            control: 0xe6e9ef,
            foreground: 0x4c4f69,
            muted: 0x6c6f85,
            border: 0xccd0da,
            accent: 0x8839ef,
            accent_foreground: 0xeff1f5,
            good: 0x40a02b,
            bad: 0xd20f39,
        },
    },
    Theme {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        dark: true,
        pair: Some("catppuccin-latte"),
        colors: Colors {
            background: 0x1e1e2e,
            control: 0x313244,
            foreground: 0xcdd6f4,
            muted: 0xa6adc8,
            border: 0x45475a,
            accent: 0xcba6f7,
            accent_foreground: 0x1e1e2e,
            good: 0xa6e3a1,
            bad: 0xf38ba8,
        },
    },
    Theme {
        id: "rose-pine-dawn",
        name: "Rosé Pine Dawn",
        dark: false,
        pair: Some("rose-pine"),
        colors: Colors {
            background: 0xfaf4ed,
            control: 0xfffaf3,
            foreground: 0x575279,
            muted: 0x797593,
            border: 0xdfdad9,
            accent: 0x286983,
            accent_foreground: 0xfaf4ed,
            good: 0x56949f,
            bad: 0xb4637a,
        },
    },
    Theme {
        id: "rose-pine",
        name: "Rosé Pine",
        dark: true,
        pair: Some("rose-pine-dawn"),
        colors: Colors {
            background: 0x191724,
            control: 0x26233a,
            foreground: 0xe0def4,
            muted: 0x908caa,
            border: 0x403d52,
            accent: 0xc4a7e7,
            accent_foreground: 0x191724,
            good: 0x9ccfd8,
            bad: 0xeb6f92,
        },
    },
];

impl Theme {
    /// A saved theme, by its id (`gui.json` may have been edited by hand).
    pub fn find(id: &str) -> Option<&'static Theme> {
        let id = id.trim();
        THEMES
            .iter()
            .find(|theme| theme.id.eq_ignore_ascii_case(id))
    }

    /// The theme to start with: the saved one (or the default), switched to the other
    /// half of its pair if that's the one that matches the system's light or dark
    /// setting. `system_dark` is `None` when the setting couldn't be found out.
    pub fn at_launch(saved: Option<&str>, system_dark: Option<bool>) -> &'static Theme {
        let theme = saved
            .and_then(Self::find)
            .or_else(|| Self::find(DEFAULT))
            .unwrap_or(&THEMES[0]);
        match system_dark {
            Some(dark) if dark != theme.dark => theme.pair.and_then(Self::find).unwrap_or(theme),
            _ => theme,
        }
    }

    /// Where Settings lists it.
    pub fn index(&self) -> usize {
        THEMES
            .iter()
            .position(|theme| theme.id == self.id)
            .unwrap_or(0)
    }

    pub fn colors(&self) -> ThemeColors {
        let c = &self.colors;
        ThemeColors {
            background: color(c.background),
            control: color(c.control),
            foreground: color(c.foreground),
            muted: color(c.muted),
            border: color(c.border),
            accent: color(c.accent),
            accent_foreground: color(c.accent_foreground),
            good: color(c.good),
            bad: color(c.bad),
        }
    }
}

fn color(rgb: u32) -> slint::Color {
    let [_, r, g, b] = rgb.to_be_bytes();
    slint::Color::from_rgb_u8(r, g, b)
}

/// Whether the system is set to dark mode right now, or `None` if that can't be found out.
pub fn system_prefers_dark() -> Option<bool> {
    let dark = system::prefers_dark();
    match dark {
        Some(true) => log::debug!("the system is in dark mode"),
        Some(false) => log::debug!("the system is in light mode"),
        None => log::debug!("couldn't tell whether the system is in light or dark mode"),
    }
    dark
}

#[cfg(windows)]
mod system {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

    /// Settings → Personalization → Colors → "Choose your default app mode".
    pub fn prefers_dark() -> Option<bool> {
        let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        let value = wide("AppsUseLightTheme");
        let mut light: u32 = 0;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: both names are NUL-terminated UTF-16, and `light` has room for the
        // DWORD asked for, as `size` says.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&raw mut light).cast(),
                &mut size,
            )
        };
        (status == ERROR_SUCCESS).then_some(light == 0)
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }
}

#[cfg(target_os = "macos")]
mod system {
    use objc2_foundation::{NSString, NSUserDefaults};

    /// System Settings → Appearance. macOS sets `AppleInterfaceStyle` to "Dark" while the
    /// appearance is dark (also when it's set to change automatically), and removes it
    /// otherwise.
    pub fn prefers_dark() -> Option<bool> {
        let style = NSUserDefaults::standardUserDefaults()
            .stringForKey(&NSString::from_str("AppleInterfaceStyle"));
        Some(style.is_some_and(|style| style.to_string().eq_ignore_ascii_case("dark")))
    }
}

#[cfg(target_os = "linux")]
mod system {
    use std::time::Duration;

    use zbus::zvariant::{OwnedValue, Value};

    /// How long the desktop portal gets to answer. It normally takes a few milliseconds;
    /// this only matters when it's broken, and then the app starts without waiting long.
    const TIMEOUT: Duration = Duration::from_millis(800);

    /// The XDG desktop portal's `color-scheme` setting, which GNOME, KDE and others keep
    /// in step with their own dark mode setting.
    pub fn prefers_dark() -> Option<bool> {
        match async_io::block_on(read_color_scheme()) {
            // 1: prefers dark, 2: prefers light, 0: no preference
            Ok(Some(1)) => Some(true),
            Ok(Some(2)) => Some(false),
            Ok(_) => None,
            Err(e) => {
                log::debug!("couldn't ask the desktop portal for the color scheme: {e}");
                None
            }
        }
    }

    async fn read_color_scheme() -> zbus::Result<Option<u32>> {
        let bus = zbus::connection::Builder::session()?
            .method_timeout(TIMEOUT)
            .build()
            .await?;
        // `ReadOne` is from version 2 of the portal; older ones only have `Read`
        let mut reply = None;
        for method in ["ReadOne", "Read"] {
            match bus
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.portal.Settings"),
                    method,
                    &("org.freedesktop.appearance", "color-scheme"),
                )
                .await
            {
                Ok(message) => {
                    reply = Some(message);
                    break;
                }
                // try the older method; anything else (a timeout, say) won't get better
                Err(zbus::Error::MethodError(..)) if method == "ReadOne" => {}
                Err(e) => return Err(e),
            }
        }
        let Some(reply) = reply else {
            return Ok(None);
        };
        let body = reply.body();
        let value: OwnedValue = body.deserialize()?;
        Ok(scheme(&value))
    }

    /// `Read` wraps the value in one more variant than `ReadOne` does.
    fn scheme(value: &Value<'_>) -> Option<u32> {
        match value {
            Value::U32(scheme) => Some(*scheme),
            Value::Value(inner) => scheme(inner),
            _ => None,
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod system {
    pub fn prefers_dark() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_are_listed_once_and_pairs_match() {
        for (i, theme) in THEMES.iter().enumerate() {
            assert_eq!(
                Theme::find(theme.id).map(Theme::index),
                Some(i),
                "{}",
                theme.id
            );
            if let Some(pair) = theme.pair {
                let other = Theme::find(pair).expect("the pair is listed");
                assert_ne!(other.dark, theme.dark, "{} and {pair}", theme.id);
                assert_eq!(other.pair, Some(theme.id), "{pair} pairs back");
            }
        }
        assert!(Theme::find(DEFAULT).is_some());
    }

    #[test]
    fn starts_in_the_half_that_matches_the_system() {
        let at_launch = |saved, dark| Theme::at_launch(saved, dark).id;

        // GitHub's to begin with
        assert_eq!(at_launch(None, Some(false)), "github-light");
        assert_eq!(at_launch(None, Some(true)), "github-dark");
        assert_eq!(at_launch(None, None), "github-light");

        // a pair follows the system, whichever half was chosen last time
        assert_eq!(
            at_launch(Some("solarized-light"), Some(true)),
            "solarized-dark"
        );
        assert_eq!(
            at_launch(Some("solarized-dark"), Some(false)),
            "solarized-light"
        );
        assert_eq!(at_launch(Some("dark"), Some(true)), "dark");
        assert_eq!(at_launch(Some("github-dark"), None), "github-dark");

        // a theme without a pair stays as it is
        assert_eq!(at_launch(Some("nord"), Some(false)), "nord");
        assert_eq!(at_launch(Some("dracula"), Some(true)), "dracula");

        // hand-edited, or from a later version
        assert_eq!(at_launch(Some(" Nord "), Some(false)), "nord");
        assert_eq!(at_launch(Some("vaporwave"), Some(true)), "github-dark");
    }
}
