//! A Domain represents an instance of a multiplexer.
//! For example, the gui frontend has its own domain,
//! and we can connect to a domain hosted by a mux server
//! that may be local, running "remotely" inside a WSL
//! container or actually remote, running on the other end
//! of an ssh session somewhere.

use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, Pane, PaneId};
use crate::tab::{SplitDirection, Tab, TabId};
use crate::window::WindowId;
use crate::Mux;
use anyhow::{bail, Error};
use async_trait::async_trait;
use config::configuration;
use downcast_rs::{impl_downcast, Downcast};
use portable_pty::{native_pty_system, CommandBuilder, PtySize, PtySystem};
use std::rc::Rc;

static DOMAIN_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type DomainId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainState {
    Detached,
    Attached,
}

pub fn alloc_domain_id() -> DomainId {
    DOMAIN_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed)
}

#[async_trait(?Send)]
pub trait Domain: Downcast {
    /// Spawn a new command within this domain
    async fn spawn(
        &self,
        size: PtySize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> Result<Rc<Tab>, Error>;

    async fn split_pane(
        &self,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        tab: TabId,
        pane_id: PaneId,
        split_direction: SplitDirection,
    ) -> anyhow::Result<Rc<dyn Pane>>;

    /// Returns false if the `spawn` method will never succeed.
    /// There are some internal placeholder domains that are
    /// pre-created with local UI that we do not want to allow
    /// to show in the launcher/menu as launchable items.
    fn spawnable(&self) -> bool {
        true
    }

    /// Returns the domain id, which is useful for obtaining
    /// a handle on the domain later.
    fn domain_id(&self) -> DomainId;

    /// Returns the name of the domain.
    /// Should be a short identifier.
    fn domain_name(&self) -> &str;

    /// Returns a label describing the domain.
    fn domain_label(&self) -> &str {
        self.domain_name()
    }

    /// Re-attach to any tabs that might be pre-existing in this domain
    async fn attach(&self) -> anyhow::Result<()>;

    /// Detach all tabs
    fn detach(&self) -> anyhow::Result<()>;

    /// Indicates the state of the domain
    fn state(&self) -> DomainState;

    /// Called to advise the domain that a local window is closing.
    /// This allows the domain the opportunity to eg: detach/hide
    /// its tabs/panes rather than actually killing them off
    fn local_window_is_closing(&self, _window_id: WindowId) {}
}
impl_downcast!(Domain);

pub struct LocalDomain {
    pty_system: Box<dyn PtySystem>,
    id: DomainId,
    name: String,
}

impl LocalDomain {
    pub fn new(name: &str) -> Result<Self, Error> {
        Ok(Self::with_pty_system(name, native_pty_system()))
    }

    pub fn with_pty_system(name: &str, pty_system: Box<dyn PtySystem>) -> Self {
        let id = alloc_domain_id();
        Self {
            pty_system,
            id,
            name: name.to_string(),
        }
    }
}

#[async_trait(?Send)]
impl Domain for LocalDomain {
    async fn spawn(
        &self,
        size: PtySize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> Result<Rc<Tab>, Error> {
        let config = configuration();
        let mut cmd = match command {
            Some(mut cmd) => {
                config.apply_cmd_defaults(&mut cmd);
                cmd
            }
            None => config.build_prog(None)?,
        };
        if let Some(dir) = command_dir {
            // I'm not normally a fan of existence checking, but not checking here
            // can be painful; in the case where a tab is local but has connected
            // to a remote system and that remote has used OSC 7 to set a path
            // that doesn't exist on the local system, process spawning can fail.
            if std::path::Path::new(&dir).exists() {
                cmd.cwd(dir);
            }
        }
        let pair = self.pty_system.openpty(size)?;
        let pane_id = alloc_pane_id();
        cmd.env("WEZTERM_PANE", pane_id.to_string());

        let child = pair.slave.spawn_command(cmd)?;
        log::trace!("spawned: {:?}", child);

        let writer = pair.master.try_clone_writer()?;
        let mux = Mux::get().unwrap();

        let terminal = wezterm_term::Terminal::new(
            crate::pty_size_to_terminal_size(size),
            std::sync::Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer),
        );

        let pane: Rc<dyn Pane> = Rc::new(LocalPane::new(
            pane_id,
            terminal,
            child,
            pair.master,
            self.id,
        ));

        let tab = Rc::new(Tab::new(&size));
        tab.assign_pane(&pane);

        mux.add_tab_and_active_pane(&tab)?;
        mux.add_tab_to_window(&tab, window)?;

        Ok(tab)
    }

    async fn split_pane(
        &self,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        tab: TabId,
        pane_id: PaneId,
        direction: SplitDirection,
    ) -> anyhow::Result<Rc<dyn Pane>> {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_tab(tab) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", tab),
        };

        let pane_index = match tab
            .iter_panes()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        let split_size = match tab.compute_split_size(pane_index, direction) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let config = configuration();
        let mut cmd = match command {
            Some(mut cmd) => {
                config.apply_cmd_defaults(&mut cmd);
                cmd
            }
            None => config.build_prog(None)?,
        };
        if let Some(dir) = command_dir {
            // I'm not normally a fan of existence checking, but not checking here
            // can be painful; in the case where a tab is local but has connected
            // to a remote system and that remote has used OSC 7 to set a path
            // that doesn't exist on the local system, process spawning can fail.
            if std::path::Path::new(&dir).exists() {
                cmd.cwd(dir);
            }
        }
        let pair = self.pty_system.openpty(split_size.second)?;
        let pane_id = alloc_pane_id();
        cmd.env("WEZTERM_PANE", pane_id.to_string());
        let child = pair.slave.spawn_command(cmd)?;
        log::trace!("spawned: {:?}", child);

        let writer = pair.master.try_clone_writer()?;

        let terminal = wezterm_term::Terminal::new(
            crate::pty_size_to_terminal_size(split_size.second),
            std::sync::Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer),
        );

        let pane: Rc<dyn Pane> = Rc::new(LocalPane::new(
            pane_id,
            terminal,
            child,
            pair.master,
            self.id,
        ));

        tab.split_and_insert(pane_index, direction, Rc::clone(&pane))?;

        mux.add_pane(&pane)?;

        Ok(pane)
    }

    fn domain_id(&self) -> DomainId {
        self.id
    }

    fn domain_name(&self) -> &str {
        &self.name
    }

    async fn attach(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn detach(&self) -> anyhow::Result<()> {
        bail!("detach not implemented");
    }

    fn state(&self) -> DomainState {
        DomainState::Attached
    }
}
