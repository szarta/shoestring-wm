//! Runtime keybinding table: compiles a [`shoestring_config::Config`]'s
//! string-named binds into (modmask, keysym) → Action lookups.

use shoestring_config::{Action, Binding, Config};
use smithay::input::keyboard::{xkb, ModifiersState};

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ModMask: u8 {
        const CTRL  = 0b0001;
        const ALT   = 0b0010;
        const SHIFT = 0b0100;
        const LOGO  = 0b1000;
    }
}

impl ModMask {
    pub fn from_state(state: &ModifiersState) -> Self {
        let mut m = ModMask::empty();
        if state.ctrl {
            m |= ModMask::CTRL;
        }
        if state.alt {
            m |= ModMask::ALT;
        }
        if state.shift {
            m |= ModMask::SHIFT;
        }
        if state.logo {
            m |= ModMask::LOGO;
        }
        m
    }

    fn parse_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "super" | "logo" | "mod4" | "win" => Some(ModMask::LOGO),
            "ctrl" | "control" => Some(ModMask::CTRL),
            "alt" | "mod1" => Some(ModMask::ALT),
            "shift" => Some(ModMask::SHIFT),
            _ => None,
        }
    }
}

pub struct CompiledBind {
    pub mods: ModMask,
    pub keysym: u32,
    pub action: Action,
}

#[derive(Default)]
pub struct BindingTable {
    binds: Vec<CompiledBind>,
}

impl BindingTable {
    pub fn compile(cfg: &Config) -> (Self, Vec<String>) {
        let mut binds = Vec::with_capacity(cfg.bindings.len());
        let mut warnings = Vec::new();
        for b in &cfg.bindings {
            match compile_one(b) {
                Ok(cb) => binds.push(cb),
                Err(why) => warnings.push(why),
            }
        }
        (Self { binds }, warnings)
    }

    /// Look up an action for the given modifiers + raw keysym. Modifier match
    /// ignores Caps/Num/etc. lock state (we already filter to the four binds).
    pub fn lookup(&self, mods: ModMask, keysym: u32) -> Option<&Action> {
        self.binds
            .iter()
            .find(|b| b.mods == mods && b.keysym == keysym)
            .map(|b| &b.action)
    }

    /// Emit one debug-level line per compiled bind so RUST_LOG=debug captures
    /// the live binding table at startup. Useful when a bind appears not to
    /// fire — confirms it was actually registered with the expected keysym.
    pub fn log_compiled(&self) {
        for b in &self.binds {
            tracing::debug!(
                keysym = b.keysym,
                keysym_name = %xkb::keysym_get_name(xkb::Keysym::new(b.keysym)),
                mods = ?b.mods,
                action = ?b.action,
                "compiled bind"
            );
        }
    }
}

fn compile_one(b: &Binding) -> Result<CompiledBind, String> {
    let mut mods = ModMask::empty();
    for name in &b.mods {
        let Some(m) = ModMask::parse_name(name) else {
            return Err(format!(
                "unknown modifier '{name}' in binding for key '{}'",
                b.key
            ));
        };
        mods |= m;
    }
    let keysym = xkb::keysym_from_name(&b.key, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym.raw() == xkb::keysyms::KEY_NoSymbol {
        return Err(format!("unknown keysym '{}'", b.key));
    }
    Ok(CompiledBind {
        mods,
        keysym: keysym.raw(),
        action: b.action.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoestring_config::{Action, Binding, Config};

    fn cfg_with(binds: Vec<Binding>) -> Config {
        Config {
            general: Default::default(),
            workspaces: Default::default(),
            bindings: binds,
            window_rules: Vec::new(),
        }
    }

    #[test]
    fn compiles_known_binding_and_looks_up() {
        let cfg = cfg_with(vec![Binding {
            mods: vec!["Super".into(), "Shift".into()],
            key: "q".into(),
            action: Action::Quit,
        }]);
        let (table, warnings) = BindingTable::compile(&cfg);
        assert!(warnings.is_empty());

        let q = xkb::keysym_from_name("q", xkb::KEYSYM_CASE_INSENSITIVE).raw();
        let action = table
            .lookup(ModMask::LOGO | ModMask::SHIFT, q)
            .expect("should find binding");
        assert!(matches!(action, Action::Quit));

        // Wrong modifier set → no match.
        assert!(table.lookup(ModMask::LOGO, q).is_none());
    }

    #[test]
    fn default_config_compiles_with_no_warnings() {
        // Catches typos in default keysym names — particularly the
        // XF86Audio* / XF86MonBrightness* media keys, which are easy
        // to misspell and only error at runtime otherwise.
        let cfg = Config::with_default_bindings();
        let (_table, warnings) = BindingTable::compile(&cfg);
        assert!(warnings.is_empty(), "default bindings warned: {warnings:?}");
    }

    #[test]
    fn unknown_modifier_or_key_warns() {
        let cfg = cfg_with(vec![
            Binding {
                mods: vec!["Hyper".into()],
                key: "q".into(),
                action: Action::Quit,
            },
            Binding {
                mods: vec!["Super".into()],
                key: "NoSuchKey".into(),
                action: Action::Quit,
            },
        ]);
        let (_table, warnings) = BindingTable::compile(&cfg);
        assert_eq!(warnings.len(), 2);
    }
}
