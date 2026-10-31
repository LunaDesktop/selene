//! `ext_workspace_v1` — lista i przełączanie przestrzeni roboczych dla powłoki.
//!
//! Kompozytor wystawia globalny menedżer; każdy klient (pasek `luna-shell`) dostaje
//! jedną **grupę** i po jednym **uchwycie** na przestrzeń roboczą. Uchwyt niesie
//! `id`/`name`/`state` (m.in. `active`), a jego żądanie `activate` przełącza przestrzeń
//! w kompozytorze (patrz [`crate::seat::switch_workspace`]) — to czyni kropki w pasku
//! w pełni funkcjonalnymi.

use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1 as group, ext_workspace_handle_v1 as workspace,
    ext_workspace_manager_v1 as manager,
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use wayland_protocols::ext::workspace::v1::server::ext_workspace_group_handle_v1::GroupCapabilities;
use wayland_protocols::ext::workspace::v1::server::ext_workspace_handle_v1::{
    State as WsState, WorkspaceCapabilities,
};

use crate::state::State;
use crate::sync::LockExt;

impl GlobalDispatch<manager::ExtWorkspaceManagerV1, ()> for State {
    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        _client: &Client,
        resource: New<manager::ExtWorkspaceManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let mgr = data_init.init(resource, ());
        announce(state, handle, &mgr);
        state.ws_managers.push(mgr);
    }
}

impl Dispatch<manager::ExtWorkspaceManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &manager::ExtWorkspaceManagerV1,
        request: manager::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Zmiany wysyłamy od razu, więc `commit` nic nie robi.
            manager::Request::Commit => {}
            manager::Request::Stop => {
                let mgr_id = resource.id();
                let client_id = resource.client().map(|c| c.id());
                state.ws_managers.retain(|m| m.id() != mgr_id);
                if let Some(client_id) = client_id {
                    state
                        .ws_groups
                        .retain(|g| g.client().map(|c| c.id() != client_id).unwrap_or(false));
                    state
                        .ws_handles
                        .retain(|h| h.client().map(|c| c.id() != client_id).unwrap_or(false));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<group::ExtWorkspaceGroupHandleV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &group::ExtWorkspaceGroupHandleV1,
        request: group::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Tworzenie przestrzeni przez klienta nie jest wspierane (stały zestaw).
            group::Request::CreateWorkspace { .. } => {}
            group::Request::Destroy => {
                let id = resource.id();
                state.ws_groups.retain(|g| g.id() != id);
            }
            _ => {}
        }
    }
}

impl Dispatch<workspace::ExtWorkspaceHandleV1, usize> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &workspace::ExtWorkspaceHandleV1,
        request: workspace::Request,
        index: &usize,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            workspace::Request::Activate => crate::seat::switch_workspace(state, *index),
            // Dezaktywacja/przypisanie/usuwanie: poza zakresem (stały zestaw przestrzeni).
            workspace::Request::Deactivate
            | workspace::Request::Assign { .. }
            | workspace::Request::Remove => {}
            workspace::Request::Destroy => {
                let id = Resource::id(resource);
                state.ws_handles.retain(|h| Resource::id(h) != id);
            }
            _ => {}
        }
    }
}

/// Synchronizuje uchwyty z aktualną `ws_count` po zmianie liczby przestrzeni.
///
/// Usuwa nadmiarowe uchwyty (`Removed`, żeby pasek zdjął kropki) i dogrywa nowe
/// dla każdego podłączonego menedżera — bez restartu Selene.
pub fn sync_workspace_handles(state: &mut State) {
    let Some(display) = state.display_handle.clone() else {
        return;
    };
    let count = state.ws_count;
    let removed: Vec<_> = state
        .ws_handles
        .iter()
        .filter(|handle| handle.data::<usize>().is_some_and(|index| *index >= count))
        .cloned()
        .collect();
    for handle in &removed {
        let _ = handle.send_event(workspace::Event::Removed);
    }
    state
        .ws_handles
        .retain(|handle| handle.data::<usize>().is_some_and(|index| *index < count));

    let managers = state.ws_managers.clone();
    let groups = state.ws_groups.clone();
    let current = *state.ws_current.lock_or_recover();
    for manager in managers {
        let Some(client) = manager.client() else {
            continue;
        };
        let client_id = client.id();
        let Some(group) = groups
            .iter()
            .find(|group| group.client().map(|client| client.id()) == Some(client_id.clone()))
            .cloned()
        else {
            continue;
        };
        let existing: Vec<usize> = state
            .ws_handles
            .iter()
            .filter(|handle| handle.client().map(|client| client.id()) == Some(client_id.clone()))
            .filter_map(|handle| handle.data::<usize>().copied())
            .collect();
        for index in 0..count {
            if existing.contains(&index) {
                continue;
            }
            let Ok(handle) = client
                .create_resource::<workspace::ExtWorkspaceHandleV1, usize, State>(
                    &display, 1, index,
                )
            else {
                continue;
            };
            let _ = manager.send_event(manager::Event::Workspace {
                workspace: handle.clone(),
            });
            let _ = handle.send_event(workspace::Event::Id {
                id: format!("ws{}", index + 1),
            });
            let _ = handle.send_event(workspace::Event::Name {
                name: state.workspace_name(index),
            });
            let _ = handle.send_event(workspace::Event::Coordinates {
                coordinates: Vec::new(),
            });
            let _ = handle.send_event(workspace::Event::Capabilities {
                capabilities: WEnum::Value(WorkspaceCapabilities::Activate),
            });
            let _ = handle.send_event(workspace::Event::State {
                state: ws_state(index, current),
            });
            let _ = group.send_event(group::Event::WorkspaceEnter {
                workspace: handle.clone(),
            });
            state.ws_handles.push(handle);
        }
        // Protokół wymaga `done` po serii zmian (dodanie/usunięcie uchwytów) —
        // klienci (pasek, podgląd) stosują zmiany dopiero po tym zdarzeniu.
        let _ = manager.send_event(manager::Event::Done);
    }
    broadcast_workspace_states(state);
}

/// Zgłasza nowo podłączonemu menedżerowi grupę i wszystkie przestrzenie.
fn announce(state: &mut State, display: &DisplayHandle, mgr: &manager::ExtWorkspaceManagerV1) {
    let Some(client) = mgr.client() else {
        return;
    };
    let Ok(group) =
        client.create_resource::<group::ExtWorkspaceGroupHandleV1, (), State>(display, 1, ())
    else {
        return;
    };
    let _ = mgr.send_event(manager::Event::WorkspaceGroup {
        workspace_group: group.clone(),
    });
    let _ = group.send_event(group::Event::Capabilities {
        capabilities: WEnum::<GroupCapabilities>::Unknown(0),
    });

    let cur = *state.ws_current.lock_or_recover();
    for index in 0..state.ws_count {
        let Ok(ws) = client
            .create_resource::<workspace::ExtWorkspaceHandleV1, usize, State>(display, 1, index)
        else {
            continue;
        };
        let _ = mgr.send_event(manager::Event::Workspace {
            workspace: ws.clone(),
        });
        let _ = ws.send_event(workspace::Event::Id {
            id: format!("ws{}", index + 1),
        });
        let _ = ws.send_event(workspace::Event::Name {
            name: state.workspace_name(index),
        });
        let _ = ws.send_event(workspace::Event::Coordinates {
            coordinates: Vec::new(),
        });
        let _ = ws.send_event(workspace::Event::Capabilities {
            capabilities: WEnum::Value(WorkspaceCapabilities::Activate),
        });
        let _ = ws.send_event(workspace::Event::State {
            state: ws_state(index, cur),
        });
        let _ = group.send_event(group::Event::WorkspaceEnter {
            workspace: ws.clone(),
        });
        state.ws_handles.push(ws);
    }
    state.ws_groups.push(group);
    let _ = mgr.send_event(manager::Event::Done);
}

/// Rozgłasza stan wszystkich przestrzeni (po przełączeniu) do podłączonych klientów.
pub fn broadcast_workspace_states(state: &State) {
    let cur = *state.ws_current.lock_or_recover();
    for h in &state.ws_handles {
        if let Some(&index) = h.data::<usize>() {
            let _ = h.send_event(workspace::Event::State {
                state: ws_state(index, cur),
            });
        }
    }
}

/// Stan przestrzeni `index`, gdy bieżąca to `cur`.
fn ws_state(index: usize, cur: usize) -> WEnum<WsState> {
    if index == cur {
        WEnum::Value(WsState::Active)
    } else {
        WEnum::Unknown(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_current_workspace_is_active() {
        assert!(matches!(ws_state(0, 0), WEnum::Value(WsState::Active)));
        assert!(matches!(ws_state(1, 0), WEnum::Unknown(0)));
        assert!(matches!(ws_state(3, 3), WEnum::Value(WsState::Active)));
    }
}
