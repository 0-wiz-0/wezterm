use crate::termwindow::{ClipboardHelper, MuxWindowId};
use anyhow::{anyhow, bail};
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::TermConfig;
use mux::activity::Activity;
use mux::domain::DomainState;
use mux::tab::SplitDirection;
use mux::Mux;
use percent_encoding::percent_decode_str;
use portable_pty::{CommandBuilder, PtySize};
use std::sync::Arc;

#[derive(Copy, Debug, Clone, Eq, PartialEq)]
pub enum SpawnWhere {
    NewWindow,
    NewTab,
    SplitPane(SplitDirection),
}

impl super::TermWindow {
    pub fn spawn_command(&mut self, spawn: &SpawnCommand, spawn_where: SpawnWhere) {
        let size = if spawn_where == SpawnWhere::NewWindow {
            self.config.initial_size()
        } else {
            self.terminal_size
        };
        let term_config = Arc::new(TermConfig::with_config(self.config.clone()));

        Self::spawn_command_impl(
            spawn,
            spawn_where,
            size,
            self.mux_window_id,
            ClipboardHelper {
                window: self.window.as_ref().unwrap().clone(),
            },
            term_config,
        )
    }

    pub fn spawn_command_impl(
        spawn: &SpawnCommand,
        spawn_where: SpawnWhere,
        size: PtySize,
        src_window_id: MuxWindowId,
        clipboard: ClipboardHelper,
        term_config: Arc<TermConfig>,
    ) {
        let spawn = spawn.clone();

        promise::spawn::spawn(async move {
            if let Err(err) = Self::spawn_command_internal(
                spawn,
                spawn_where,
                size,
                src_window_id,
                clipboard,
                term_config,
            )
            .await
            {
                log::error!("Failed to spawn: {:#}", err);
            }
        })
        .detach();
    }

    async fn spawn_command_internal(
        spawn: SpawnCommand,
        spawn_where: SpawnWhere,
        size: PtySize,
        src_window_id: MuxWindowId,
        clipboard: ClipboardHelper,
        term_config: Arc<TermConfig>,
    ) -> anyhow::Result<()> {
        let mux = Mux::get().unwrap();
        let activity = Activity::new();
        let mux_builder;

        let target_window_id = if spawn_where == SpawnWhere::NewWindow {
            mux_builder = mux.new_empty_window();
            *mux_builder
        } else {
            src_window_id
        };

        let domain = match (&spawn.domain, spawn_where) {
            (SpawnTabDomain::DefaultDomain, _)
            // CurrentPaneDomain is the default value for the spawn domain.
            // It doesn't make sense to use it when spawning a new window,
            // so we treat it as DefaultDomain instead.
            | (SpawnTabDomain::CurrentPaneDomain, SpawnWhere::NewWindow) => {
                mux.default_domain().clone()
            }
            (SpawnTabDomain::CurrentPaneDomain, _) => {
                let tab = match mux.get_active_tab_for_window(src_window_id) {
                    Some(tab) => tab,
                    None => bail!("window has no tabs?"),
                };
                let pane = tab
                    .get_active_pane()
                    .ok_or_else(|| anyhow!("current tab has no pane!?"))?;
                mux.get_domain(pane.domain_id())
                    .ok_or_else(|| anyhow!("current tab has unresolvable domain id!?"))?
            }
            (SpawnTabDomain::DomainName(name), _) => {
                mux.get_domain_by_name(&name).ok_or_else(|| {
                    anyhow!("spawn_tab called with unresolvable domain name {}", name)
                })?
            }
            (SpawnTabDomain::DomainId(domain_id), _) => {
                mux.get_domain(*domain_id).ok_or_else(|| {
                    anyhow!("spawn_tab called with unresolvable domain id {}", domain_id)
                })?
            }
        };

        if domain.state() == DomainState::Detached {
            bail!("Cannot spawn a tab into a Detached domain");
        }

        let cwd = if let Some(cwd) = spawn.cwd.as_ref() {
            Some(cwd.to_str().map(|s| s.to_owned()).ok_or_else(|| {
                anyhow!(
                    "Domain::spawn requires that the cwd be unicode in {:?}",
                    cwd
                )
            })?)
        } else {
            let cwd = match (spawn.domain, spawn_where) {
                (SpawnTabDomain::DefaultDomain, _)
                // CurrentPaneDomain is the default value for the spawn domain.
                // It doesn't make sense to use it when spawning a new window,
                // so we treat it as DefaultDomain instead.
                | (SpawnTabDomain::CurrentPaneDomain, SpawnWhere::NewWindow) => mux
                    .get_active_tab_for_window(src_window_id)
                    .and_then(|tab| tab.get_active_pane())
                    .and_then(|pane| pane.get_current_working_dir()),
                (SpawnTabDomain::CurrentPaneDomain, _) => {
                    let tab = match mux.get_active_tab_for_window(src_window_id) {
                        Some(tab) => tab,
                        None => bail!("window has no tabs?"),
                    };
                    let pane = tab
                        .get_active_pane()
                        .ok_or_else(|| anyhow!("current tab has no pane!?"))?;
                    pane.get_current_working_dir()
                }
                _ => None,
            };

            match cwd {
                Some(url) if url.scheme() == "file" => {
                    if let Ok(path) = percent_decode_str(url.path()).decode_utf8() {
                        let path = path.into_owned();
                        // On Windows the file URI can produce a path like:
                        // `/C:\Users` which is valid in a file URI, but the leading slash
                        // is not liked by the windows file APIs, so we strip it off here.
                        let bytes = path.as_bytes();
                        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
                            Some(path[1..].to_owned())
                        } else {
                            Some(path)
                        }
                    } else {
                        None
                    }
                }
                Some(_) | None => None,
            }
        };

        let cmd_builder = if let Some(args) = spawn.args {
            let mut builder = CommandBuilder::from_argv(args.iter().map(Into::into).collect());
            for (k, v) in spawn.set_environment_variables.iter() {
                builder.env(k, v);
            }
            if let Some(cwd) = spawn.cwd {
                builder.cwd(cwd);
            }
            Some(builder)
        } else {
            None
        };

        let clipboard: Arc<dyn wezterm_term::Clipboard> = Arc::new(clipboard);
        let downloader: Arc<dyn wezterm_term::DownloadHandler> =
            Arc::new(crate::download::Downloader::new());

        match spawn_where {
            SpawnWhere::SplitPane(direction) => {
                let mux = Mux::get().unwrap();
                if let Some(tab) = mux.get_active_tab_for_window(target_window_id) {
                    let pane = tab
                        .get_active_pane()
                        .ok_or_else(|| anyhow!("tab to have a pane"))?;

                    log::trace!("doing split_pane");
                    let pane = domain
                        .split_pane(cmd_builder, cwd, tab.tab_id(), pane.pane_id(), direction)
                        .await?;
                    pane.set_config(term_config);
                    pane.set_clipboard(&clipboard);
                    pane.set_download_handler(&downloader);
                } else {
                    bail!("there is no active tab while splitting pane!?");
                }
            }
            _ => {
                let tab = domain
                    .spawn(size, cmd_builder, cwd, target_window_id)
                    .await?;
                let tab_id = tab.tab_id();
                let pane = tab
                    .get_active_pane()
                    .ok_or_else(|| anyhow!("newly spawned tab to have a pane"))?;
                pane.set_config(term_config);

                if spawn_where != SpawnWhere::NewWindow {
                    pane.set_clipboard(&clipboard);
                    pane.set_download_handler(&downloader);
                    let mut window = mux
                        .get_window_mut(target_window_id)
                        .ok_or_else(|| anyhow!("no such window!?"))?;
                    if let Some(idx) = window.idx_by_id(tab_id) {
                        window.save_and_then_set_active(idx);
                    }
                }
            }
        };

        drop(activity);

        Ok(())
    }

    pub fn spawn_tab(&mut self, domain: &SpawnTabDomain) {
        self.spawn_command(
            &SpawnCommand {
                domain: domain.clone(),
                ..Default::default()
            },
            SpawnWhere::NewTab,
        );
    }
}
