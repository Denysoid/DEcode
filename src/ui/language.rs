use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, ListPicker,
        ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};

use crate::config::UiLanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageFocus {
    Languages,
    Apply,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageHit {
    Language(usize),
    Apply,
    Close,
}

#[derive(Debug, Clone)]
pub struct LanguageUiState {
    open: bool,
    dialog: DialogState<()>,
    picker: ListPickerState,
    focus: FocusManager<LanguageFocus>,
    clicks: ClickRegionRegistry<LanguageHit>,
}

impl LanguageUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(LanguageFocus::Languages);
        focus.register(LanguageFocus::Apply);
        focus.register(LanguageFocus::Close);
        focus.set(LanguageFocus::Languages);
        Self {
            open: false,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(UiLanguage::ALL.len()),
            focus,
            clicks: ClickRegionRegistry::new(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, current: UiLanguage) {
        let selected = UiLanguage::ALL
            .iter()
            .position(|language| *language == current)
            .unwrap_or(0);
        self.picker.set_total(UiLanguage::ALL.len());
        self.picker.select(selected);
        self.focus.set(LanguageFocus::Languages);
        self.open = true;
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    #[must_use]
    pub fn selected_language(&self) -> UiLanguage {
        UiLanguage::ALL
            .get(self.picker.selected_index)
            .copied()
            .unwrap_or_default()
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.focus.set(LanguageFocus::Languages);
    }

    pub fn next(&mut self) {
        self.picker.select_next();
    }

    pub fn previous(&mut self) {
        self.picker.select_prev();
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: LanguageFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<LanguageFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<LanguageHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, current: UiLanguage) {
        if !self.open {
            return;
        }
        let selected = self.picker.selected_index;
        let focused = self.focus.current().copied();
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(copy(current, Copy::Title))
            .width_percent(56)
            .height_percent(70)
            .min_size(52, 22)
            .max_size(90, 34)
            .border_color(Color::Blue)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_language_dialog(frame, area, current, selected, focused, picker, clicks);
        });
        popup.render(frame);
    }
}

impl Default for LanguageUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_language_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    current: UiLanguage,
    _selected: usize,
    focused: Option<LanguageFocus>,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<LanguageHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(12),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                copy(current, Copy::Heading),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(copy(current, Copy::Description)),
            Line::from(copy(current, Copy::Hint)),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(LanguageFocus::Languages) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::Gray)
        })
        .title(format!(" {} ", copy(current, Copy::List)));
    let inner = block.inner(rows[1]);
    frame.render_widget(block, rows[1]);
    let labels = UiLanguage::ALL
        .iter()
        .map(|language| format!("{}  [{}]", language.label(), language.code()))
        .collect::<Vec<_>>();
    picker.ensure_visible(usize::from(inner.height));
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for row in 0..usize::from(inner.height) {
        let index = usize::from(picker.scroll).saturating_add(row);
        if index >= UiLanguage::ALL.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            LanguageHit::Language(index),
        );
    }

    let buttons = Layout::horizontal([
        Constraint::Length(22),
        Constraint::Length(2),
        Constraint::Length(22),
        Constraint::Fill(1),
    ])
    .split(rows[2]);
    draw_button(
        frame,
        buttons[0],
        copy(current, Copy::Apply),
        focused == Some(LanguageFocus::Apply),
        LanguageHit::Apply,
        clicks,
    );
    draw_button(
        frame,
        buttons[2],
        copy(current, Copy::Close),
        focused == Some(LanguageFocus::Close),
        LanguageHit::Close,
        clicks,
    );
}

fn draw_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    hit: LanguageHit,
    clicks: &mut ClickRegionRegistry<LanguageHit>,
) {
    let mut state = ButtonState::enabled();
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(area, frame.buffer_mut());
    clicks.register(region.area, hit);
}

#[derive(Clone, Copy)]
enum Copy {
    Title,
    Heading,
    Description,
    Hint,
    List,
    Apply,
    Close,
}

fn copy(language: UiLanguage, key: Copy) -> &'static str {
    let values = match key {
        Copy::Title => [
            "Interface language",
            "Язык интерфейса",
            "Мова інтерфейсу",
            "Idioma de la interfaz",
            "Oberflächensprache",
            "Langue de l’interface",
            "Język interfejsu",
            "Idioma da interface",
            "界面语言",
            "インターフェース言語",
            "인터페이스 언어",
            "Arayüz dili",
        ],
        Copy::Heading => [
            "Choose a language",
            "Выберите язык",
            "Оберіть мову",
            "Elige un idioma",
            "Sprache wählen",
            "Choisissez une langue",
            "Wybierz język",
            "Escolha um idioma",
            "选择语言",
            "言語を選択",
            "언어 선택",
            "Dil seçin",
        ],
        Copy::Description => [
            "The change applies immediately to the whole interface and is remembered for future launches.",
            "Изменение сразу применяется ко всему интерфейсу и запоминается для следующих запусков.",
            "Зміна одразу застосовується до всього інтерфейсу та зберігається для наступних запусків.",
            "El cambio se aplica a toda la interfaz y se recuerda para futuros inicios.",
            "Die Änderung gilt sofort für die gesamte Oberfläche und wird gespeichert.",
            "Le changement s’applique immédiatement à toute l’interface et sera mémorisé.",
            "Zmiana obejmie cały interfejs i zostanie zapamiętana.",
            "A alteração vale para toda a interface e será lembrada.",
            "更改会立即应用到整个界面，并在以后启动时保留。",
            "変更は画面全体にすぐ適用され、次回起動時も保持されます。",
            "변경 사항은 전체 인터페이스에 즉시 적용되고 다음 실행에도 유지됩니다.",
            "Değişiklik tüm arayüze hemen uygulanır ve sonraki açılışlar için saklanır.",
        ],
        Copy::Hint => [
            "Mouse: click a row and Apply · Keyboard: arrows, Tab, Enter, Esc",
            "Мышь: язык и Применить · Клавиатура: стрелки, Tab, Enter, Esc",
            "Миша: мова й Застосувати · Клавіатура: стрілки, Tab, Enter, Esc",
            "Ratón: idioma y Aplicar · Teclado: flechas, Tab, Enter, Esc",
            "Maus: Sprache und Anwenden · Tastatur: Pfeile, Tab, Enter, Esc",
            "Souris : langue puis Appliquer · Clavier : flèches, Tab, Entrée, Échap",
            "Mysz: język i Zastosuj · Klawiatura: strzałki, Tab, Enter, Esc",
            "Mouse: idioma e Aplicar · Teclado: setas, Tab, Enter, Esc",
            "鼠标：选择语言并应用 · 键盘：方向键、Tab、Enter、Esc",
            "マウス：言語を選び適用 · キーボード：矢印、Tab、Enter、Esc",
            "마우스: 언어 선택 후 적용 · 키보드: 화살표, Tab, Enter, Esc",
            "Fare: dili seçip Uygula · Klavye: oklar, Tab, Enter, Esc",
        ],
        Copy::List => [
            "Languages",
            "Языки",
            "Мови",
            "Idiomas",
            "Sprachen",
            "Langues",
            "Języki",
            "Idiomas",
            "语言",
            "言語",
            "언어",
            "Diller",
        ],
        Copy::Apply => [
            "✓ Apply",
            "✓ Применить",
            "✓ Застосувати",
            "✓ Aplicar",
            "✓ Anwenden",
            "✓ Appliquer",
            "✓ Zastosuj",
            "✓ Aplicar",
            "✓ 应用",
            "✓ 適用",
            "✓ 적용",
            "✓ Uygula",
        ],
        Copy::Close => [
            "× Close (Esc)",
            "× Закрыть (Esc)",
            "× Закрити (Esc)",
            "× Cerrar (Esc)",
            "× Schließen (Esc)",
            "× Fermer (Esc)",
            "× Zamknij (Esc)",
            "× Fechar (Esc)",
            "× 关闭 (Esc)",
            "× 閉じる (Esc)",
            "× 닫기 (Esc)",
            "× Kapat (Esc)",
        ],
    };
    let index = UiLanguage::ALL
        .iter()
        .position(|candidate| *candidate == language)
        .unwrap_or(0);
    values[index.min(values.len().saturating_sub(1))]
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn infallible<T>(result: Result<T, Infallible>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => match error {},
        }
    }

    #[test]
    fn language_rows_and_both_actions_are_mouse_clickable() {
        let backend = TestBackend::new(90, 30);
        let mut terminal = infallible(Terminal::new(backend));
        let mut state = LanguageUiState::new();
        state.open(UiLanguage::English);
        infallible(terminal.draw(|frame| {
            state.begin_frame();
            state.draw(frame, UiLanguage::English);
        }));
        let mut hits = Vec::new();
        for row in 0..30 {
            for column in 0..90 {
                if let Some(hit) = state.clicked(column, row) {
                    hits.push(hit);
                }
            }
        }
        assert!(hits.contains(&LanguageHit::Apply));
        assert!(hits.contains(&LanguageHit::Close));
        assert!(
            hits.iter()
                .filter(|hit| matches!(hit, LanguageHit::Language(_)))
                .count()
                >= UiLanguage::ALL.len()
        );
    }
}
