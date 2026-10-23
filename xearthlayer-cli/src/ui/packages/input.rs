//! Keys to actions, by tab and open modal.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{Action, EditKey, Modal, Tab};

/// The action a key asks for on `tab`, or `None` if it means nothing there.
///
/// With a modal open only its own keys apply: `y` or Enter confirms, `n` or
/// Esc dismisses, `q` confirms the quit prompt, and help closes on `?` or Esc.
/// With no modal, Esc gives `Dismiss`, which asks to quit but never abandons
/// a cancelling quit the way `q` does. Ctrl+C always quits.
pub fn action_for(key: KeyEvent, tab: Tab, modal: Option<&Modal>, editing: bool) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'));
        return ctrl_c.then_some(Action::Quit);
    }
    if editing && modal.is_none() {
        return edit_action(key.code);
    }
    match modal {
        None => tab_action(key.code, tab),
        Some(Modal::Help) => match key.code {
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Esc => Some(Action::Dismiss),
            _ => None,
        },
        Some(modal) => match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Action::Dismiss),
            KeyCode::Char('q' | 'Q') if matches!(modal, Modal::ConfirmQuit { .. }) => {
                Some(Action::Quit)
            }
            _ => None,
        },
    }
}

/// Keys with no modal open. Some act only on the tabs they belong to.
/// Keys while a setting is edited: every printable key is text, so `q`, `j`
/// and digits are typed rather than acted on.
fn edit_action(code: KeyCode) -> Option<Action> {
    let key = match code {
        KeyCode::Enter => return Some(Action::Confirm),
        KeyCode::Esc => return Some(Action::Dismiss),
        KeyCode::Char(c) => EditKey::Char(c),
        KeyCode::Backspace => EditKey::Backspace,
        KeyCode::Delete => EditKey::Delete,
        KeyCode::Left => EditKey::Left,
        KeyCode::Right => EditKey::Right,
        KeyCode::Home => EditKey::Home,
        KeyCode::End => EditKey::End,
        _ => return None,
    };
    Some(Action::EditKey(key))
}

fn tab_action(code: KeyCode, tab: Tab) -> Option<Action> {
    let on = |tabs: &[Tab], action: Action| tabs.contains(&tab).then_some(action);
    let lists = [Tab::Local, Tab::Library];
    match code {
        KeyCode::Char('1') => Some(Action::GoTab(Tab::Local)),
        KeyCode::Char('2') => Some(Action::GoTab(Tab::Library)),
        KeyCode::Char('3') => Some(Action::GoTab(Tab::Progress)),
        KeyCode::Char('4') => Some(Action::GoTab(Tab::Settings)),
        KeyCode::Enter => on(&[Tab::Settings], Action::Edit),
        KeyCode::Tab => Some(Action::NextTab),
        KeyCode::BackTab => Some(Action::PrevTab),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::Down),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::Home => Some(Action::Home),
        KeyCode::End => Some(Action::End),
        KeyCode::Char(' ') => on(&lists, Action::Mark),
        KeyCode::Char('a') => on(&lists, Action::MarkAll),
        KeyCode::Char('t') => on(&[Tab::Library], Action::CycleFilter),
        KeyCode::Char('u') => on(&[Tab::Local], Action::Update),
        KeyCode::Char('d') => on(&[Tab::Local], Action::Delete),
        KeyCode::Char('i') => on(&[Tab::Library], Action::Install),
        KeyCode::Char('c') => on(&[Tab::Progress], Action::Cancel),
        KeyCode::Char('C') => on(&[Tab::Progress], Action::CancelAll),
        KeyCode::Char('x') => on(&[Tab::Progress], Action::ClearFinished),
        KeyCode::Char('r') => Some(Action::Refresh),
        KeyCode::Char('?') => Some(Action::Help),
        KeyCode::Char('q' | 'Q') => Some(Action::Quit),
        // With nothing to dismiss, `update` treats it as asking to quit.
        KeyCode::Esc => Some(Action::Dismiss),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::*;
    use crate::ui::packages::app::{Action, Modal, Tab};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        let modifiers = if c.is_ascii_uppercase() || c == '?' {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        KeyEvent::new(KeyCode::Char(c), modifiers)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    const TABS: [Tab; 3] = [Tab::Local, Tab::Library, Tab::Progress];

    fn quit_modal() -> Modal {
        Modal::ConfirmQuit {
            since: Instant::now(),
            running: 0,
        }
    }

    fn confirm_modals() -> Vec<Modal> {
        vec![
            Modal::ConfirmDelete {
                targets: Vec::new(),
                total_bytes: 0,
            },
            Modal::ConfirmInstall {
                entries: Vec::new(),
                skipped: Vec::new(),
                replacing: Vec::new(),
                notes: Vec::new(),
            },
            Modal::ConfirmCancelAll,
        ]
    }

    // ------------------------------------------------------------------------
    // No modal
    // ------------------------------------------------------------------------

    #[test]
    fn global_keys_act_on_every_tab() {
        let keys = [
            (ch('1'), Action::GoTab(Tab::Local)),
            (ch('2'), Action::GoTab(Tab::Library)),
            (ch('3'), Action::GoTab(Tab::Progress)),
            (press(KeyCode::Tab), Action::NextTab),
            (
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                Action::PrevTab,
            ),
            (press(KeyCode::Up), Action::Up),
            (ch('k'), Action::Up),
            (press(KeyCode::Down), Action::Down),
            (ch('j'), Action::Down),
            (press(KeyCode::PageUp), Action::PageUp),
            (press(KeyCode::PageDown), Action::PageDown),
            (press(KeyCode::Home), Action::Home),
            (press(KeyCode::End), Action::End),
            (ch('r'), Action::Refresh),
            (ch('?'), Action::Help),
            (ch('q'), Action::Quit),
            (press(KeyCode::Esc), Action::Dismiss),
            (ctrl('c'), Action::Quit),
        ];
        for tab in TABS {
            for (key, action) in &keys {
                assert_eq!(
                    action_for(*key, tab, None, false),
                    Some(*action),
                    "{key:?} on {tab:?}"
                );
            }
        }
    }

    #[test]
    fn tab_keys_act_only_on_their_tabs() {
        let keys = [
            (ch(' '), Action::Mark, &[Tab::Local, Tab::Library][..]),
            (ch('a'), Action::MarkAll, &[Tab::Local, Tab::Library][..]),
            (ch('t'), Action::CycleFilter, &[Tab::Library][..]),
            (ch('u'), Action::Update, &[Tab::Local][..]),
            (ch('d'), Action::Delete, &[Tab::Local][..]),
            (ch('i'), Action::Install, &[Tab::Library][..]),
            (ch('c'), Action::Cancel, &[Tab::Progress][..]),
            (ch('C'), Action::CancelAll, &[Tab::Progress][..]),
            (ch('x'), Action::ClearFinished, &[Tab::Progress][..]),
        ];
        for (key, action, tabs) in keys {
            for tab in TABS {
                let expected = tabs.contains(&tab).then_some(action);
                assert_eq!(
                    action_for(key, tab, None, false),
                    expected,
                    "{key:?} on {tab:?}"
                );
            }
        }
    }

    #[test]
    fn unbound_and_control_keys_do_nothing() {
        for key in [
            ch('z'),
            ch('y'),
            press(KeyCode::Enter),
            ctrl('a'),
            ctrl('q'),
        ] {
            assert_eq!(action_for(key, Tab::Local, None, false), None, "{key:?}");
        }
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut key = ch('q');
        key.kind = KeyEventKind::Release;

        assert_eq!(action_for(key, Tab::Local, None, false), None);
    }

    // ------------------------------------------------------------------------
    // Modals
    // ------------------------------------------------------------------------

    #[test]
    fn a_confirmation_takes_only_yes_and_no() {
        let mut modals = confirm_modals();
        modals.push(quit_modal());
        for modal in &modals {
            for key in [ch('y'), ch('Y'), press(KeyCode::Enter)] {
                assert_eq!(
                    action_for(key, Tab::Local, Some(modal), false),
                    Some(Action::Confirm),
                    "{key:?} in {modal:?}"
                );
            }
            for key in [ch('n'), ch('N'), press(KeyCode::Esc)] {
                assert_eq!(
                    action_for(key, Tab::Local, Some(modal), false),
                    Some(Action::Dismiss),
                    "{key:?} in {modal:?}"
                );
            }
            for key in [ch('j'), ch('2'), ch('?'), ch('d'), press(KeyCode::Down)] {
                assert_eq!(
                    action_for(key, Tab::Local, Some(modal), false),
                    None,
                    "{key:?} in {modal:?}"
                );
            }
        }
    }

    #[test]
    fn q_quits_only_inside_the_quit_confirmation() {
        let quit = quit_modal();
        assert_eq!(
            action_for(ch('q'), Tab::Local, Some(&quit), false),
            Some(Action::Quit)
        );
        for modal in &confirm_modals() {
            assert_eq!(
                action_for(ch('q'), Tab::Local, Some(modal), false),
                None,
                "{modal:?}"
            );
        }
    }

    #[test]
    fn help_closes_on_question_mark_or_esc() {
        let help = Modal::Help;

        assert_eq!(
            action_for(ch('?'), Tab::Local, Some(&help), false),
            Some(Action::Help)
        );
        assert_eq!(
            action_for(press(KeyCode::Esc), Tab::Local, Some(&help), false),
            Some(Action::Dismiss)
        );
        for key in [ch('y'), ch('n'), ch('q'), press(KeyCode::Enter), ch('j')] {
            assert_eq!(
                action_for(key, Tab::Local, Some(&help), false),
                None,
                "{key:?}"
            );
        }
    }

    #[test]
    fn ctrl_c_quits_under_any_modal() {
        let mut modals = confirm_modals();
        modals.push(quit_modal());
        modals.push(Modal::Help);
        for modal in &modals {
            assert_eq!(
                action_for(ctrl('c'), Tab::Progress, Some(modal), false),
                Some(Action::Quit),
                "{modal:?}"
            );
        }
    }

    #[test]
    fn four_opens_settings_and_enter_edits_only_there() {
        assert_eq!(
            action_for(ch('4'), Tab::Local, None, false),
            Some(Action::GoTab(Tab::Settings))
        );
        assert_eq!(
            action_for(press(KeyCode::Enter), Tab::Settings, None, false),
            Some(Action::Edit)
        );
        for tab in [Tab::Local, Tab::Library, Tab::Progress] {
            assert_eq!(
                action_for(press(KeyCode::Enter), tab, None, false),
                None,
                "{tab:?}"
            );
        }
    }

    #[test]
    fn while_editing_every_key_is_text_except_enter_esc_and_ctrl_c() {
        use crate::ui::packages::app::EditKey;
        let editing = |key| action_for(key, Tab::Settings, None, true);
        for c in ['q', 'j', '4', '?', ' ', 'r'] {
            assert_eq!(
                editing(ch(c)),
                Some(Action::EditKey(EditKey::Char(c))),
                "{c}"
            );
        }
        assert_eq!(editing(press(KeyCode::Enter)), Some(Action::Confirm));
        assert_eq!(editing(press(KeyCode::Esc)), Some(Action::Dismiss));
        assert_eq!(
            editing(press(KeyCode::Backspace)),
            Some(Action::EditKey(EditKey::Backspace))
        );
        assert_eq!(
            editing(press(KeyCode::Left)),
            Some(Action::EditKey(EditKey::Left))
        );
        assert_eq!(editing(ctrl('c')), Some(Action::Quit), "Ctrl+C still quits");
        assert_eq!(editing(press(KeyCode::Up)), None);
    }
}
