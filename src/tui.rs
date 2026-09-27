use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEventKind,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};

use crate::{
    db::{Database, Terminal, Workspace},
    terminal::TerminalManager,
};

#[derive(Clone)]
enum Item {
    Workspace(Workspace),
    Terminal(Terminal),
}

pub fn run(database: &Database, manager: &TerminalManager) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive mode requires a terminal; use `webterm list` for plain output")
    }
    let mut screen = Screen::enter()?;
    let result = run_loop(&mut screen, database, manager);
    screen.leave()?;
    result
}

fn run_loop(screen: &mut Screen, database: &Database, manager: &TerminalManager) -> Result<()> {
    let workspaces = database.list_workspaces()?;
    let terminals = database.list_terminals(None)?;
    let mut expanded: HashSet<i64> = workspaces.iter().map(|workspace| workspace.id).collect();
    let mut items = hierarchy(&workspaces, &terminals, &expanded);
    let mut selected = 0usize;
    let mut message = String::from("↑/↓ navigate • Enter attach • click a terminal • q quit");

    loop {
        if !items.is_empty() {
            selected = selected.min(items.len() - 1);
        }
        let (_, height) = size()?;
        let visible = usize::from(height.saturating_sub(4)).max(1);
        let offset = selected.saturating_sub(visible - 1);
        draw(screen, &items, &expanded, selected, offset, &message)?;

        match event::read().context("read terminal input")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected + 1 < items.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter | KeyCode::Char(' ') => activate(
                    screen,
                    items.get(selected).cloned(),
                    manager,
                    &mut expanded,
                    &workspaces,
                    &terminals,
                    &mut items,
                    &mut message,
                )?,
                _ => {}
            },
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                let row = usize::from(mouse.row.saturating_sub(2));
                let index = offset + row;
                if mouse.row >= 2 && index < items.len() {
                    selected = index;
                    activate(
                        screen,
                        items.get(selected).cloned(),
                        manager,
                        &mut expanded,
                        &workspaces,
                        &terminals,
                        &mut items,
                        &mut message,
                    )?;
                }
            }
            _ => {}
        }
    }
}

fn draw(
    screen: &mut Screen,
    items: &[Item],
    expanded: &HashSet<i64>,
    selected: usize,
    offset: usize,
    message: &str,
) -> Result<()> {
    let (width, height) = size()?;
    queue!(
        screen.stdout,
        MoveTo(0, 0),
        Clear(ClearType::All),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print("webterm"),
        SetAttribute(Attribute::Reset),
        ResetColor,
        MoveTo(0, 1),
        Print("Workspaces and persistent terminals")
    )?;

    let visible = usize::from(height.saturating_sub(4)).max(1);
    for (row, (index, item)) in items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        queue!(screen.stdout, MoveTo(0, row as u16 + 2))?;
        if index == selected {
            queue!(screen.stdout, SetAttribute(Attribute::Reverse))?;
        }
        let text = match item {
            Item::Workspace(workspace) => {
                let marker = if expanded.contains(&workspace.id) {
                    "▾"
                } else {
                    "▸"
                };
                format!("{marker} {}  {}", workspace.name, workspace.path.display())
            }
            Item::Terminal(terminal) => format!(
                "    {} {}  {}  {}",
                if terminal.status == "running" {
                    "●"
                } else {
                    "○"
                },
                terminal.name,
                terminal.status,
                terminal.backend()
            ),
        };
        queue!(screen.stdout, Print(truncate_line(&text, width)))?;
        if index == selected {
            queue!(screen.stdout, SetAttribute(Attribute::Reset))?;
        }
    }

    if height > 0 {
        queue!(
            screen.stdout,
            MoveTo(0, height - 1),
            SetForegroundColor(Color::Yellow),
            Print(truncate_line(message, width)),
            ResetColor
        )?;
    }
    screen.stdout.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn activate(
    screen: &mut Screen,
    item: Option<Item>,
    manager: &TerminalManager,
    expanded: &mut HashSet<i64>,
    workspaces: &[Workspace],
    terminals: &[Terminal],
    items: &mut Vec<Item>,
    message: &mut String,
) -> Result<()> {
    match item {
        Some(Item::Workspace(workspace)) => {
            if !expanded.remove(&workspace.id) {
                expanded.insert(workspace.id);
            }
            *items = hierarchy(workspaces, terminals, expanded);
        }
        Some(Item::Terminal(terminal)) if terminal.status == "running" => {
            screen.leave()?;
            // Native runtime attach handles raw passthrough and Ctrl-] detach;
            // the managed shell remains alive after the client disconnects.
            let result = manager.attach(terminal.session_id());
            screen.resume()?;
            *message = match result {
                Ok(()) => format!("Detached from {}", terminal.name),
                Err(error) => format!("Attach failed: {error:#}"),
            };
        }
        Some(Item::Terminal(terminal)) => {
            *message = format!(
                "{} is stopped; create a new terminal to run it",
                terminal.name
            );
        }
        None => {}
    }
    Ok(())
}

fn hierarchy(
    workspaces: &[Workspace],
    terminals: &[Terminal],
    expanded: &HashSet<i64>,
) -> Vec<Item> {
    let mut items = Vec::new();
    for workspace in workspaces {
        items.push(Item::Workspace(workspace.clone()));
        if expanded.contains(&workspace.id) {
            items.extend(
                terminals
                    .iter()
                    .filter(|terminal| terminal.workspace_id == workspace.id)
                    .cloned()
                    .map(Item::Terminal),
            );
        }
    }
    items
}

fn truncate_line(value: &str, width: u16) -> String {
    value.chars().take(width as usize).collect()
}

struct Screen {
    stdout: io::Stdout,
    active: bool,
}

impl Screen {
    fn enter() -> Result<Self> {
        let mut screen = Self {
            stdout: io::stdout(),
            active: false,
        };
        screen.resume()?;
        Ok(screen)
    }

    fn resume(&mut self) -> Result<()> {
        if !self.active {
            enable_raw_mode()?;
            execute!(
                self.stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                Hide,
                Clear(ClearType::All)
            )?;
            self.active = true;
        }
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        if self.active {
            disable_raw_mode()?;
            execute!(self.stdout, Show, DisableMouseCapture, LeaveAlternateScreen)?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn hierarchy_nests_terminals_under_their_workspace() {
        let workspaces = vec![Workspace {
            id: 1,
            name: "demo".into(),
            path: PathBuf::from("/demo"),
            created_at: 0,
            updated_at: 0,
        }];
        let terminals = vec![Terminal {
            id: 2,
            workspace_id: 1,
            name: "shell".into(),
            tmux_session: "pty-test".into(),
            status: "running".into(),
            created_at: 0,
            updated_at: 0,
        }];
        let items = hierarchy(&workspaces, &terminals, &HashSet::from([1]));
        assert!(matches!(
            items.as_slice(),
            [Item::Workspace(_), Item::Terminal(_)]
        ));
    }
}
