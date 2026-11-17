use std::collections::HashSet;

use crate::{
    mail::{Mail, parents, roots},
    settings, theme,
    widgets::mail_card::MailCard,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use rataflow::{
    Background, BackgroundVariant, Edge, EdgeStyle, FitViewOptions, Flow, FlowAction, FlowEvent,
    Handle, HandlePosition, MiniMap, MiniMapPosition, Node, Rect as FlowRect, SelectionReveal,
    StepEdge, Sugiyama,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders},
};

/// Rows a card uses besides the body preview: borders, subject, sender, a gap and
/// the footer.
const CARD_CHROME_HEIGHT: usize = 6;
const HORIZONTAL_LAYOUT_WIDTH: u16 = 104;
/// Space around the first and last cards, and between neighbouring cards.
const SNAKE_MARGIN: f64 = 3.0;
const SNAKE_GAP_X: f64 = 4.0;
const SNAKE_GAP_Y: f64 = 3.0;
/// Columns the viewport moves for each shift+arrow press.
const PAN_STEP: f64 = 6.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowOrientation {
    Horizontal,
    Vertical,
}

/// How a conversation is arranged: the usual flow to the right, or a snake that
/// wraps through rows and reverses direction on each row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Arrangement {
    Adaptive(FlowOrientation),
    Snake(usize),
}

/// How a conversation is arranged and the size of its cards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CardLayout {
    arrangement: Arrangement,
    card_width: u16,
    preview_lines: u16,
}

impl CardLayout {
    fn from_settings(area_width: u16) -> Self {
        let (snake, card_width, preview_lines) = settings::with(|ui| {
            (
                ui.workspace_layout == "snake",
                ui.workspace_card_width,
                ui.workspace_preview_lines,
            )
        });
        Self {
            arrangement: arrangement(snake, area_width, card_width),
            card_width,
            preview_lines,
        }
    }

    fn width(self) -> f64 {
        f64::from(self.card_width)
    }

    fn height(self) -> f64 {
        card_height(self.preview_lines) as f64
    }
}

pub struct Workspace {
    flow: Flow<MailCard, StepEdge>,
    mails: Vec<Mail>,
    selected_mail_id: String,
    layout: CardLayout,
}

/// The viewport delta for a pan key, if this key pans.
fn pan_offset(key: KeyEvent) -> Option<(f64, f64)> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::ALT) => {
            match character.to_ascii_lowercase() {
                'h' => Some((PAN_STEP, 0.0)),
                'l' => Some((-PAN_STEP, 0.0)),
                'k' => Some((0.0, PAN_STEP)),
                'j' => Some((0.0, -PAN_STEP)),
                _ => None,
            }
        }
        KeyCode::Left if shift => Some((PAN_STEP, 0.0)),
        KeyCode::Right if shift => Some((-PAN_STEP, 0.0)),
        KeyCode::Up if shift => Some((0.0, PAN_STEP)),
        KeyCode::Down if shift => Some((0.0, -PAN_STEP)),
        _ => None,
    }
}

impl Workspace {
    pub fn new(mails: &[Mail], selected_mail_id: &str) -> Self {
        // The first draw knows the viewport width and reflows the cards.
        let layout = CardLayout::from_settings(0);
        let selected_mail_id = newest_mail_id(mails, selected_mail_id);
        Self {
            flow: conversation_flow(mails, &selected_mail_id, layout),
            mails: mails.to_vec(),
            selected_mail_id,
            layout,
        }
    }

    pub fn show_conversation(&mut self, mails: &[Mail], selected_mail_id: &str) {
        self.mails = mails.to_vec();
        self.selected_mail_id = newest_mail_id(&self.mails, selected_mail_id);
        self.flow = conversation_flow(&self.mails, &self.selected_mail_id, self.layout);
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let title = crate::i18n::t(if focused {
            " workspace • active "
        } else {
            " workspace "
        });
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::border(focused))
            .title_style(theme::title(focused))
            .title(title);
        let inner = block.inner(area);
        let layout = CardLayout::from_settings(inner.width);
        if layout != self.layout {
            self.layout = layout;
            self.flow = conversation_flow(&self.mails, &self.selected_mail_id, layout);
        }

        frame.render_widget(block, area);
        frame.render_widget(
            Background::new(&self.flow)
                .variant(BackgroundVariant::Dots)
                .gap(6, 3),
            inner,
        );
        frame.render_widget(&mut self.flow, inner);
        if self.needs_minimap(inner) {
            let map = MiniMap::new(&self.flow)
                .position(MiniMapPosition::BottomRight)
                .size(20, 7)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::border(false))
                        .title_style(theme::muted())
                        .title(crate::i18n::t(" map ")),
                );
            frame.render_widget(map, inner);
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<String> {
        let clicked =
            self.flow
                .handle_mouse_event(mouse)
                .into_events()
                .find_map(|event| match event {
                    FlowEvent::NodeClicked { node_id } => Some(node_id),
                    _ => None,
                });
        if let Some(mail_id) = &clicked {
            self.selected_mail_id = mail_id.clone();
            self.update_edge_styles();
        }
        clicked
    }

    /// Moves the viewport with shift+arrows or alt+hjkl, so a large
    /// conversation can be read without a mouse, and selects nodes with the
    /// plain keys.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some((dx, dy)) = pan_offset(key) {
            self.flow.pan(dx, dy);
            return;
        }
        match key.code {
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                // Through `apply`, so the viewport follows the selection.
                self.flow.apply(FlowAction::SelectPrev);
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('l') => {
                self.flow.apply(FlowAction::SelectNext);
            }
            KeyCode::Char('+' | '=') => self.flow.zoom_in(),
            KeyCode::Char('-' | '_') => self.flow.zoom_out(),
            KeyCode::Char('0') => self.flow.reset_zoom(),
            KeyCode::Char('f') => self.flow.request_fit_view(),
            KeyCode::Char('c') => self.flow.center_on_selected(),
            _ => {}
        }
        if let Some(mail_id) = self.flow.first_selected_node_id() {
            self.selected_mail_id = mail_id;
            self.update_edge_styles();
        }
    }

    pub fn selected_mail_id(&self) -> Option<String> {
        self.flow.first_selected_node_id()
    }

    fn needs_minimap(&self, area: Rect) -> bool {
        if area.width < 48 || area.height < 12 {
            return false;
        }

        let bounds = self
            .flow
            .nodes()
            .map(Node::bounds)
            .reduce(|a, b| a.union(&b));
        let Some(bounds) = bounds else {
            return false;
        };
        let zoom = self.flow.viewport.zoom;

        bounds.width() * zoom > area.width.saturating_sub(4) as f64
            || bounds.height() * zoom > area.height.saturating_sub(4) as f64
    }

    fn update_edge_styles(&mut self) {
        let path = selected_path_ids(&self.mails, &self.selected_mail_id);
        for (edge_id, content) in self.flow.edges_content_mut() {
            let target_id = edge_id.strip_prefix("reply-").unwrap_or(edge_id);
            content.style = Some(reply_style(path.contains(target_id)));
        }
    }
}

fn conversation_flow(
    mails: &[Mail],
    selected_mail_id: &str,
    layout: CardLayout,
) -> Flow<MailCard, StepEdge> {
    let conversation = conversation_indices(mails, selected_mail_id);
    if conversation.is_empty() {
        return Flow::new();
    }
    let parents = parents(mails);
    let selected_path = selected_path_ids(mails, selected_mail_id);
    let conversation: Vec<(&Mail, Option<&str>)> = conversation
        .into_iter()
        .map(|index| {
            (
                &mails[index],
                parents[index].map(|parent| mails[parent].id.as_str()),
            )
        })
        .collect();
    let rects: Vec<FlowRect> = match layout.arrangement {
        Arrangement::Adaptive(_) => {
            vec![
                FlowRect::from_coords(0.0, 0.0, layout.width(), layout.height());
                conversation.len()
            ]
        }
        Arrangement::Snake(columns) => (0..conversation.len())
            .map(|index| snake_rect(index, columns, layout))
            .collect(),
    };
    let indices: std::collections::HashMap<&str, usize> = conversation
        .iter()
        .enumerate()
        .map(|(index, (mail, _))| (mail.id.as_str(), index))
        .collect();
    let nodes = conversation
        .iter()
        .enumerate()
        .map(|(index, (mail, _))| {
            let rect = rects[index];
            let node = Node::new(
                &mail.id,
                rect.position,
                rect.dimensions,
                MailCard::new(mail, index + 1, conversation.len()),
            );
            let node = match layout.arrangement {
                Arrangement::Adaptive(orientation) => node
                    .with_source_position(orientation.source_side())
                    .with_target_position(orientation.target_side()),
                Arrangement::Snake(_) => node.with_handles(side_handles()),
            };
            node.with_selected(mail.id == selected_mail_id)
        })
        .collect();
    let edges: Vec<Edge<StepEdge>> = conversation
        .iter()
        .enumerate()
        .filter_map(|(index, (mail, parent_id))| {
            let parent_id = (*parent_id)?;
            let parent_index = *indices.get(parent_id)?;
            let from = rects[parent_index];
            let to = rects[index];
            let edge = Edge::new(format!("reply-{}", mail.id), parent_id, &mail.id);
            let edge = match layout.arrangement {
                Arrangement::Adaptive(orientation) => edge
                    .with_source_side(orientation.source_side())
                    .with_target_side(orientation.target_side()),
                Arrangement::Snake(_) => edge
                    .with_source_side(HandlePosition::facing(&from, to.center()))
                    .with_target_side(HandlePosition::facing(&to, from.center())),
            };
            Some(
                edge.with_content(
                    StepEdge::default()
                        .with_stem_length(2.0)
                        .with_style(reply_style(selected_path.contains(mail.id.as_str()))),
                ),
            )
        })
        .collect();

    // A malformed thread (such as two messages sharing an ID) shows an empty map rather
    // than taking the client down.
    let Ok(flow) = Flow::with_graph(nodes, edges) else {
        return Flow::new();
    };
    let mut flow = flow
        .with_theme(theme::flow())
        .with_selection_reveal(SelectionReveal::EnsureVisible)
        .with_min_zoom(0.8)
        .with_max_zoom(1.25);
    if let Arrangement::Adaptive(orientation) = layout.arrangement {
        let sugiyama = match orientation {
            FlowOrientation::Horizontal => Sugiyama::horizontal(),
            FlowOrientation::Vertical => Sugiyama::vertical(),
        }
        .with_node_spacing(3.0)
        .with_rank_spacing(4.0)
        .with_margin(3.0);
        flow.apply_layout(sugiyama);
    }
    flow.request_fit_view_with_options(
        FitViewOptions::default()
            .with_padding(2.0)
            .with_zoom_range(0.8, 1.0),
    );
    flow
}

impl FlowOrientation {
    fn source_side(self) -> HandlePosition {
        match self {
            Self::Horizontal => HandlePosition::Right,
            Self::Vertical => HandlePosition::Bottom,
        }
    }

    fn target_side(self) -> HandlePosition {
        self.source_side().opposite()
    }
}

/// Uses the usual flow unless the config asks for a snake.
fn arrangement(snake: bool, area_width: u16, card_width: u16) -> Arrangement {
    if snake {
        return Arrangement::Snake(snake_columns(area_width, card_width));
    }
    let orientation = if area_width < HORIZONTAL_LAYOUT_WIDTH {
        FlowOrientation::Vertical
    } else {
        FlowOrientation::Horizontal
    };
    Arrangement::Adaptive(orientation)
}

/// How many cards fit side by side in the map.
fn snake_columns(area_width: u16, card_width: u16) -> usize {
    let card_width = usize::from(card_width).max(1);
    let margin = SNAKE_MARGIN as usize * 2;
    let gap = SNAKE_GAP_X as usize;
    let available = usize::from(area_width).saturating_sub(margin);
    ((available + gap) / (card_width + gap)).max(1)
}

/// The place of a card in the snake: rows run left to right, then right to left.
fn snake_rect(index: usize, columns: usize, layout: CardLayout) -> FlowRect {
    let columns = columns.max(1);
    let row = index / columns;
    let offset = index % columns;
    let column = if row.is_multiple_of(2) {
        offset
    } else {
        columns - 1 - offset
    };
    FlowRect::from_coords(
        SNAKE_MARGIN + column as f64 * (layout.width() + SNAKE_GAP_X),
        SNAKE_MARGIN + row as f64 * (layout.height() + SNAKE_GAP_Y),
        layout.width(),
        layout.height(),
    )
}

/// A handle on every side, so each edge can leave and arrive facing its neighbour.
fn side_handles() -> Vec<Handle> {
    HandlePosition::ALL
        .into_iter()
        .flat_map(|side| {
            let id = side.side_name();
            [
                Handle::source(side).with_id(id).with_hidden(true),
                Handle::target(side).with_id(id).with_hidden(true),
            ]
        })
        .collect()
}

/// The height of a card with room for `preview_lines` rows of message text.
fn card_height(preview_lines: u16) -> usize {
    CARD_CHROME_HEIGHT + usize::from(preview_lines)
}

fn reply_style(active: bool) -> EdgeStyle {
    let style = Style::default().fg(if active {
        theme::accent()
    } else {
        theme::muted_color()
    });
    let style = if active {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };
    EdgeStyle::default().with_stroke_style(style)
}

/// Indices of the selected message's whole conversation, oldest message first.
fn conversation_indices(mails: &[Mail], selected_mail_id: &str) -> Vec<usize> {
    let Some(selected) = mails.iter().position(|mail| mail.id == selected_mail_id) else {
        return Vec::new();
    };
    let parents = parents(mails);
    let roots = roots(&parents);
    (0..mails.len())
        .filter(|index| roots[*index] == roots[selected])
        .collect()
}

/// Messages are ordered oldest to newest, so the last one in the chain is the
/// newest. It is the card a conversation should open on.
fn newest_mail_id(mails: &[Mail], selected_mail_id: &str) -> String {
    conversation_indices(mails, selected_mail_id)
        .into_iter()
        .last()
        .and_then(|index| mails.get(index))
        .map(|mail| mail.id.clone())
        .unwrap_or_else(|| selected_mail_id.into())
}

fn selected_path_ids<'a>(mails: &'a [Mail], selected_mail_id: &str) -> HashSet<&'a str> {
    let parents = parents(mails);
    let mut ids = HashSet::new();
    let mut current = mails.iter().position(|mail| mail.id == selected_mail_id);

    while let Some(index) = current {
        if !ids.insert(mails[index].id.as_str()) {
            break;
        }
        current = parents[index];
    }

    ids
}

#[cfg(test)]
mod tests {
    use crate::mail::demo_mails;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{
        Arrangement, CardLayout, FlowOrientation, Workspace, arrangement, card_height,
        conversation_flow, newest_mail_id, selected_path_ids, snake_columns,
    };

    fn layout(arrangement: Arrangement) -> CardLayout {
        CardLayout {
            arrangement,
            card_width: 40,
            preview_lines: 3,
        }
    }

    #[test]
    fn sizes_cards_for_the_configured_preview() {
        assert_eq!(card_height(0), 6);
        assert_eq!(card_height(3), 9);
        assert_eq!(card_height(8), 14);
    }

    #[test]
    fn keeps_the_usual_presentation_unless_snake_is_chosen() {
        assert_eq!(
            arrangement(false, 200, 40),
            Arrangement::Adaptive(FlowOrientation::Horizontal)
        );
        assert_eq!(
            arrangement(false, 80, 40),
            Arrangement::Adaptive(FlowOrientation::Vertical)
        );
        assert_eq!(arrangement(true, 104, 40), Arrangement::Snake(2));
    }

    #[test]
    fn counts_the_cards_that_fit_side_by_side() {
        assert_eq!(snake_columns(40, 40), 1);
        assert_eq!(snake_columns(104, 40), 2);
        assert_eq!(snake_columns(200, 40), 4);
    }

    #[test]
    fn lays_out_conversations_horizontally() {
        let flow = conversation_flow(
            &demo_mails(),
            "project-2",
            layout(Arrangement::Adaptive(FlowOrientation::Horizontal)),
        );

        assert!(
            flow.node("project-1").unwrap().position.x < flow.node("project-2").unwrap().position.x
        );
        assert_eq!(
            flow.node("project-1").unwrap().position.y,
            flow.node("project-2").unwrap().position.y
        );
        assert!(flow.node("project-2").unwrap().selected);
    }

    #[test]
    fn lays_out_conversations_vertically() {
        let flow = conversation_flow(
            &demo_mails(),
            "project-2",
            layout(Arrangement::Adaptive(FlowOrientation::Vertical)),
        );

        assert!(
            flow.node("project-1").unwrap().position.y < flow.node("project-2").unwrap().position.y
        );
    }

    #[test]
    fn wraps_long_chains_like_a_snake() {
        let flow = conversation_flow(&demo_mails(), "project-3", layout(Arrangement::Snake(2)));
        let first = flow.node("project-1").unwrap();
        let second = flow.node("project-2").unwrap();
        let third = flow.node("project-3").unwrap();

        assert!(first.position.x < second.position.x);
        assert_eq!(first.position.y, second.position.y);
        assert_eq!(second.position.x, third.position.x);
        assert!(third.position.y > second.position.y);
        assert_eq!(
            flow.edge("reply-project-3")
                .unwrap()
                .source_handle
                .as_deref(),
            Some("bottom")
        );
        assert_eq!(
            flow.edge("reply-project-3")
                .unwrap()
                .target_handle
                .as_deref(),
            Some("top")
        );
    }

    #[test]
    fn follows_the_selected_reply_path() {
        let mails = demo_mails();
        let path = selected_path_ids(&mails, "project-3");

        assert_eq!(path.len(), 3);
        assert!(path.contains("project-1"));
        assert!(path.contains("project-3"));
        assert!(!path.contains("weekend-1"));
    }

    #[test]
    fn opens_a_chain_on_its_newest_mail() {
        let mails = demo_mails();
        assert_eq!(newest_mail_id(&mails, "project-1"), "project-3");
        assert_eq!(newest_mail_id(&mails, "project-2"), "project-3");
        assert_eq!(newest_mail_id(&mails, "weekend-1"), "weekend-2");
        assert_eq!(
            Workspace::new(&mails, "project-1")
                .selected_mail_id()
                .as_deref(),
            Some("project-3")
        );
    }

    #[test]
    fn show_conversation_also_opens_on_the_newest_mail() {
        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "weekend-1");
        assert_eq!(workspace.selected_mail_id().as_deref(), Some("weekend-2"));

        workspace.show_conversation(&mails, "project-2");
        assert_eq!(workspace.selected_mail_id().as_deref(), Some("project-3"));
    }

    #[test]
    fn newest_mail_is_selected_in_both_arrangements() {
        let mails = demo_mails();
        for arrangement in [
            Arrangement::Adaptive(FlowOrientation::Horizontal),
            Arrangement::Adaptive(FlowOrientation::Vertical),
            Arrangement::Snake(2),
        ] {
            let flow = conversation_flow(&mails, "project-3", layout(arrangement));
            assert!(flow.node("project-3").unwrap().selected);
            assert!(!flow.node("project-1").unwrap().selected);
        }
    }

    #[test]
    fn renders_reference_branches() {
        let mut mails = demo_mails();
        mails.push(
            crate::mail::Mail::new(
                "branch",
                "mert@example.com",
                "re: project update",
                "ship it",
                None,
            )
            .with_references(&["project-1"]),
        );
        let flow = conversation_flow(
            &mails,
            "branch",
            layout(Arrangement::Adaptive(FlowOrientation::Horizontal)),
        );

        assert!(flow.edge("reply-branch").is_some());
        assert!(flow.node("project-1").is_some());
    }

    #[test]
    fn shifts_the_viewport_with_shift_arrows() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();

        let before = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        let after = workspace.flow.viewport.offset();

        assert!(
            after.x < before.x,
            "shift+right should reveal the right side"
        );
        assert_eq!(workspace.selected_mail_id().as_deref(), Some("project-3"));
    }

    #[test]
    fn pans_with_alt_hjkl() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();

        let before = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT));
        let after_h = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT));
        let after_l = workspace.flow.viewport.offset();
        // Alt+shift+h arrives as an uppercase letter on some terminals.
        workspace.handle_key(KeyEvent::new(
            KeyCode::Char('H'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        let after_shift = workspace.flow.viewport.offset();

        assert!(after_h.x > before.x, "alt+h should reveal the left side");
        assert!(after_l.x < after_h.x, "alt+l should reveal the right side");
        assert!(
            after_shift.x > after_l.x,
            "alt+shift+h should still pan left"
        );
        assert_eq!(workspace.selected_mail_id().as_deref(), Some("project-3"));
    }

    #[test]
    fn leaves_shift_j_and_shift_k_alone() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();

        let before = workspace.flow.viewport.offset();
        workspace.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT));
        workspace.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT));
        let after = workspace.flow.viewport.offset();

        assert_eq!(after.x, before.x);
        assert_eq!(after.y, before.y);
    }

    #[test]
    fn renders_mail_card_hierarchy() {
        let mails = demo_mails();
        let mut workspace = Workspace::new(&mails, "project-3");
        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area(), true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(screen.contains("workspace"));
        assert!(screen.contains("project update"), "{screen}");
        assert!(screen.contains("ada@example.com"));
        assert!(screen.contains("↵ open"));
    }
}
