//! Provides `language`-related settings.

use crate::{File, LanguageName, LanguageServerName};
use collections::{FxHashMap, HashMap, HashSet};
use ec4rs::{
    property::{FinalNewline, IndentSize, IndentStyle, MaxLineLen, TabWidth, TrimTrailingWs},
    Properties as EditorconfigProperties,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use gpui::{App, Modifiers, SharedString};
use itertools::{Either, Itertools};

pub use settings::{
    CompletionSettingsContent, FormatOnSave, Formatter, FormatterList, InlayHintKind,
    LanguageSettingsContent, LspInsertMode, RewrapBehavior, SelectedFormatter,
    ShowWhitespaceSetting, SoftWrap, WordsCompletionMode,
};
use settings::{ExtendingVec, Settings, SettingsContent, SettingsLocation, SettingsStore};
use std::{borrow::Cow, num::NonZeroU32, sync::Arc};

/// Initializes the language settings.
pub fn init(cx: &mut App) {
    AllLanguageSettings::register(cx);
}

/// Returns the settings for the specified language from the provided file.
pub fn language_settings<'a>(
    language: Option<LanguageName>,
    file: Option<&'a Arc<dyn File>>,
    cx: &'a App,
) -> Cow<'a, LanguageSettings> {
    let location = file.map(|f| SettingsLocation {
        worktree_id: f.worktree_id(cx),
        path: f.path().as_ref(),
    });
    AllLanguageSettings::get(location, cx).language(location, language.as_ref(), cx)
}

/// Returns the settings for all languages from the provided file.
pub fn all_language_settings<'a>(
    file: Option<&'a Arc<dyn File>>,
    cx: &'a App,
) -> &'a AllLanguageSettings {
    let location = file.map(|f| SettingsLocation {
        worktree_id: f.worktree_id(cx),
        path: f.path().as_ref(),
    });
    AllLanguageSettings::get(location, cx)
}

/// The settings for all languages.
#[derive(Debug, Clone)]
pub struct AllLanguageSettings {
    /// The edit prediction settings.
    pub defaults: LanguageSettings,
    languages: HashMap<LanguageName, LanguageSettings>,
    pub(crate) file_types: FxHashMap<Arc<str>, GlobSet>,
}

#[derive(Debug, Clone)]
pub struct WhitespaceMap {
    pub space: SharedString,
    pub tab: SharedString,
}

/// The settings for a particular language.
#[derive(Debug, Clone)]
pub struct LanguageSettings {
    /// How many columns a tab should occupy.
    pub tab_size: NonZeroU32,
    /// Whether to indent lines using tab characters, as opposed to multiple
    /// spaces.
    pub hard_tabs: bool,
    /// How to soft-wrap long lines of text.
    pub soft_wrap: settings::SoftWrap,
    /// The column at which to soft-wrap lines, for buffers where soft-wrap
    /// is enabled.
    pub preferred_line_length: u32,
    /// Whether to show wrap guides (vertical rulers) in the editor.
    /// Setting this to true will show a guide at the 'preferred_line_length' value
    /// if softwrap is set to 'preferred_line_length', and will show any
    /// additional guides as specified by the 'wrap_guides' setting.
    pub show_wrap_guides: bool,
    /// Character counts at which to show wrap guides (vertical rulers) in the editor.
    pub wrap_guides: Vec<usize>,
    /// Indent guide related settings.
    pub indent_guides: IndentGuideSettings,
    /// Whether or not to perform a buffer format before saving.
    pub format_on_save: FormatOnSave,
    /// Whether or not to remove any trailing whitespace from lines of a buffer
    /// before saving it.
    pub remove_trailing_whitespace_on_save: bool,
    /// Whether or not to ensure there's a single newline at the end of a buffer
    /// when saving it.
    pub ensure_final_newline_on_save: bool,
    /// How to perform a buffer format.
    pub formatter: settings::SelectedFormatter,
    /// Zed's Prettier integration settings.
    pub prettier: PrettierSettings,
    /// Whether to automatically close JSX tags.
    pub jsx_tag_auto_close: bool,
    /// Whether to use language servers to provide code intelligence.
    pub enable_language_server: bool,
    /// The list of language servers to use (or disable) for this language.
    ///
    /// This array should consist of language server IDs, as well as the following
    /// special tokens:
    /// - `"!<language_server_id>"` - A language server ID prefixed with a `!` will be disabled.
    /// - `"..."` - A placeholder to refer to the **rest** of the registered language servers for this language.
    pub language_servers: Vec<String>,
    /// Controls where the `editor::Rewrap` action is allowed for this language.
    ///
    /// Note: This setting has no effect in Vim mode, as rewrap is already
    /// allowed everywhere.
    pub allow_rewrap: RewrapBehavior,
    /// Whether to show tabs and spaces in the editor.
    pub show_whitespaces: settings::ShowWhitespaceSetting,
    /// Visible characters used to render whitespace when show_whitespaces is enabled.
    pub whitespace_map: WhitespaceMap,
    /// Whether to start a new line with a comment when a previous line is a comment as well.
    pub extend_comment_on_newline: bool,
    /// Inlay hint related settings.
    pub inlay_hints: InlayHintSettings,
    /// Whether to automatically close brackets.
    pub use_autoclose: bool,
    /// Whether to automatically surround text with brackets.
    pub use_auto_surround: bool,
    /// Whether to use additional LSP queries to format (and amend) the code after
    /// every "trigger" symbol input, defined by LSP server capabilities.
    pub use_on_type_format: bool,
    /// Whether indentation should be adjusted based on the context whilst typing.
    pub auto_indent: bool,
    /// Whether indentation of pasted content should be adjusted based on the context.
    pub auto_indent_on_paste: bool,
    /// Controls how the editor handles the autoclosed characters.
    pub always_treat_brackets_as_autoclosed: bool,
    /// Which code actions to run on save
    pub code_actions_on_format: HashMap<String, bool>,
    /// Whether to perform linked edits
    pub linked_edits: bool,
    /// Task configuration for this language.
    pub tasks: LanguageTaskSettings,
    /// Whether to pop the completions menu while typing in an editor without
    /// explicitly requesting it.
    pub show_completions_on_input: bool,
    /// Whether to display inline and alongside documentation for items in the
    /// completions menu.
    pub show_completion_documentation: bool,
    /// Completion settings for this language.
    pub completions: CompletionSettings,
    /// Preferred debuggers for this language.
    pub debuggers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CompletionSettings {
    /// Controls how words are completed.
    /// For large documents, not all words may be fetched for completion.
    ///
    /// Default: `fallback`
    pub words: WordsCompletionMode,
    /// How many characters has to be in the completions query to automatically show the words-based completions.
    /// Before that value, it's still possible to trigger the words-based completion manually with the corresponding editor command.
    ///
    /// Default: 3
    pub words_min_length: usize,
    /// Whether to fetch LSP completions or not.
    ///
    /// Default: true
    pub lsp: bool,
    /// When fetching LSP completions, determines how long to wait for a response of a particular server.
    /// When set to 0, waits indefinitely.
    ///
    /// Default: 0
    pub lsp_fetch_timeout_ms: u64,
    /// Controls how LSP completions are inserted.
    ///
    /// Default: "replace_suffix"
    pub lsp_insert_mode: LspInsertMode,
}

/// The settings for indent guides.
#[derive(Debug, Clone, PartialEq)]
pub struct IndentGuideSettings {
    /// Whether to display indent guides in the editor.
    ///
    /// Default: true
    pub enabled: bool,
    /// The width of the indent guides in pixels, between 1 and 10.
    ///
    /// Default: 1
    pub line_width: u32,
    /// The width of the active indent guide in pixels, between 1 and 10.
    ///
    /// Default: 1
    pub active_line_width: u32,
    /// Determines how indent guides are colored.
    ///
    /// Default: Fixed
    pub coloring: settings::IndentGuideColoring,
    /// Determines how indent guide backgrounds are colored.
    ///
    /// Default: Disabled
    pub background_coloring: settings::IndentGuideBackgroundColoring,
}

#[derive(Debug, Clone)]
pub struct LanguageTaskSettings {
    /// Extra task variables to set for a particular language.
    pub variables: HashMap<String, String>,
    pub enabled: bool,
    /// Use LSP tasks over Zed language extension ones.
    /// If no LSP tasks are returned due to error/timeout or regular execution,
    /// Zed language extension tasks will be used instead.
    ///
    /// Other Zed tasks will still be shown:
    /// * Zed task from either of the task config file
    /// * Zed task from history (e.g. one-off task was spawned before)
    pub prefer_lsp: bool,
}

/// Allows to enable/disable formatting with Prettier
/// and configure default Prettier, used when no project-level Prettier installation is found.
/// Prettier formatting is disabled by default.
#[derive(Debug, Clone)]
pub struct PrettierSettings {
    /// Enables or disables formatting with Prettier for a given language.
    pub allowed: bool,

    /// Forces Prettier integration to use a specific parser name when formatting files with the language.
    pub parser: Option<String>,

    /// Forces Prettier integration to use specific plugins when formatting files with the language.
    /// The default Prettier will be installed with these plugins.
    pub plugins: HashSet<String>,

    /// Default Prettier options, in the format as in package.json section for Prettier.
    /// If project installs Prettier via its package.json, these options will be ignored.
    pub options: HashMap<String, serde_json::Value>,
}

impl LanguageSettings {
    /// A token representing the rest of the available language servers.
    const REST_OF_LANGUAGE_SERVERS: &'static str = "...";

    /// Returns the customized list of language servers from the list of
    /// available language servers.
    pub fn customized_language_servers(
        &self,
        available_language_servers: &[LanguageServerName],
    ) -> Vec<LanguageServerName> {
        Self::resolve_language_servers(&self.language_servers, available_language_servers)
    }

    pub(crate) fn resolve_language_servers(
        configured_language_servers: &[String],
        available_language_servers: &[LanguageServerName],
    ) -> Vec<LanguageServerName> {
        let (disabled_language_servers, enabled_language_servers): (
            Vec<LanguageServerName>,
            Vec<LanguageServerName>,
        ) = configured_language_servers.iter().partition_map(
            |language_server| match language_server.strip_prefix('!') {
                Some(disabled) => Either::Left(LanguageServerName(disabled.to_string().into())),
                None => Either::Right(LanguageServerName(language_server.clone().into())),
            },
        );

        let rest = available_language_servers
            .iter()
            .filter(|&available_language_server| {
                !disabled_language_servers.contains(available_language_server)
                    && !enabled_language_servers.contains(available_language_server)
            })
            .cloned()
            .collect::<Vec<_>>();

        enabled_language_servers
            .into_iter()
            .flat_map(|language_server| {
                if language_server.0.as_ref() == Self::REST_OF_LANGUAGE_SERVERS {
                    rest.clone()
                } else {
                    vec![language_server]
                }
            })
            .collect::<Vec<_>>()
    }
}

// The settings for inlay hints.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InlayHintSettings {
    /// Global switch to toggle hints on and off.
    ///
    /// Default: false
    pub enabled: bool,
    /// Global switch to toggle inline values on and off when debugging.
    ///
    /// Default: true
    pub show_value_hints: bool,
    /// Whether type hints should be shown.
    ///
    /// Default: true
    pub show_type_hints: bool,
    /// Whether parameter hints should be shown.
    ///
    /// Default: true
    pub show_parameter_hints: bool,
    /// Whether other hints should be shown.
    ///
    /// Default: true
    pub show_other_hints: bool,
    /// Whether to show a background for inlay hints.
    ///
    /// If set to `true`, the background will use the `hint.background` color
    /// from the current theme.
    ///
    /// Default: false
    pub show_background: bool,
    /// Whether or not to debounce inlay hints updates after buffer edits.
    ///
    /// Set to 0 to disable debouncing.
    ///
    /// Default: 700
    pub edit_debounce_ms: u64,
    /// Whether or not to debounce inlay hints updates after buffer scrolls.
    ///
    /// Set to 0 to disable debouncing.
    ///
    /// Default: 50
    pub scroll_debounce_ms: u64,
    /// Toggles inlay hints (hides or shows) when the user presses the modifiers specified.
    /// If only a subset of the modifiers specified is pressed, hints are not toggled.
    /// If no modifiers are specified, this is equivalent to `None`.
    ///
    /// Default: None
    pub toggle_on_modifiers_press: Option<Modifiers>,
}

impl InlayHintSettings {
    /// Returns the kinds of inlay hints that are enabled based on the settings.
    pub fn enabled_inlay_hint_kinds(&self) -> HashSet<Option<InlayHintKind>> {
        let mut kinds = HashSet::default();
        if self.show_type_hints {
            kinds.insert(Some(InlayHintKind::Type));
        }
        if self.show_parameter_hints {
            kinds.insert(Some(InlayHintKind::Parameter));
        }
        if self.show_other_hints {
            kinds.insert(None);
        }
        kinds
    }
}

impl AllLanguageSettings {
    /// Returns the [`LanguageSettings`] for the language with the specified name.
    pub fn language<'a>(
        &'a self,
        location: Option<SettingsLocation<'a>>,
        language_name: Option<&LanguageName>,
        cx: &'a App,
    ) -> Cow<'a, LanguageSettings> {
        let settings = language_name
            .and_then(|name| self.languages.get(name))
            .unwrap_or(&self.defaults);

        let editorconfig_properties = location.and_then(|location| {
            cx.global::<SettingsStore>()
                .editorconfig_properties(location.worktree_id, location.path)
        });
        if let Some(editorconfig_properties) = editorconfig_properties {
            let mut settings = settings.clone();
            merge_with_editorconfig(&mut settings, &editorconfig_properties);
            Cow::Owned(settings)
        } else {
            Cow::Borrowed(settings)
        }
    }
}

fn merge_with_editorconfig(settings: &mut LanguageSettings, cfg: &EditorconfigProperties) {
    let preferred_line_length = cfg.get::<MaxLineLen>().ok().and_then(|v| match v {
        MaxLineLen::Value(u) => Some(u as u32),
        MaxLineLen::Off => None,
    });
    let tab_size = cfg.get::<IndentSize>().ok().and_then(|v| match v {
        IndentSize::Value(u) => NonZeroU32::new(u as u32),
        IndentSize::UseTabWidth => cfg.get::<TabWidth>().ok().and_then(|w| match w {
            TabWidth::Value(u) => NonZeroU32::new(u as u32),
        }),
    });
    let hard_tabs = cfg
        .get::<IndentStyle>()
        .map(|v| v.eq(&IndentStyle::Tabs))
        .ok();
    let ensure_final_newline_on_save = cfg
        .get::<FinalNewline>()
        .map(|v| match v {
            FinalNewline::Value(b) => b,
        })
        .ok();
    let remove_trailing_whitespace_on_save = cfg
        .get::<TrimTrailingWs>()
        .map(|v| match v {
            TrimTrailingWs::Value(b) => b,
        })
        .ok();
    fn merge<T>(target: &mut T, value: Option<T>) {
        if let Some(value) = value {
            *target = value;
        }
    }
    merge(&mut settings.preferred_line_length, preferred_line_length);
    merge(&mut settings.tab_size, tab_size);
    merge(&mut settings.hard_tabs, hard_tabs);
    merge(
        &mut settings.remove_trailing_whitespace_on_save,
        remove_trailing_whitespace_on_save,
    );
    merge(
        &mut settings.ensure_final_newline_on_save,
        ensure_final_newline_on_save,
    );
}

impl settings::Settings for AllLanguageSettings {
    fn from_settings(content: &settings::SettingsContent, _cx: &mut App) -> Self {
        let all_languages = &content.project.all_languages;

        fn load_from_content(settings: LanguageSettingsContent) -> LanguageSettings {
            let inlay_hints = settings.inlay_hints.unwrap();
            let completions = settings.completions.unwrap();
            let prettier = settings.prettier.unwrap();
            let indent_guides = settings.indent_guides.unwrap();
            let tasks = settings.tasks.unwrap();
            let whitespace_map = settings.whitespace_map.unwrap();

            LanguageSettings {
                tab_size: settings.tab_size.unwrap(),
                hard_tabs: settings.hard_tabs.unwrap(),
                soft_wrap: settings.soft_wrap.unwrap(),
                preferred_line_length: settings.preferred_line_length.unwrap(),
                show_wrap_guides: settings.show_wrap_guides.unwrap(),
                wrap_guides: settings.wrap_guides.unwrap(),
                indent_guides: IndentGuideSettings {
                    enabled: indent_guides.enabled.unwrap(),
                    line_width: indent_guides.line_width.unwrap(),
                    active_line_width: indent_guides.active_line_width.unwrap(),
                    coloring: indent_guides.coloring.unwrap(),
                    background_coloring: indent_guides.background_coloring.unwrap(),
                },
                format_on_save: settings.format_on_save.unwrap(),
                remove_trailing_whitespace_on_save: settings
                    .remove_trailing_whitespace_on_save
                    .unwrap(),
                ensure_final_newline_on_save: settings.ensure_final_newline_on_save.unwrap(),
                formatter: settings.formatter.unwrap(),
                prettier: PrettierSettings {
                    allowed: prettier.allowed.unwrap(),
                    parser: prettier.parser,
                    plugins: prettier.plugins,
                    options: prettier.options,
                },
                jsx_tag_auto_close: settings.jsx_tag_auto_close.unwrap().enabled.unwrap(),
                enable_language_server: settings.enable_language_server.unwrap(),
                language_servers: settings.language_servers.unwrap(),
                allow_rewrap: settings.allow_rewrap.unwrap(),
                show_whitespaces: settings.show_whitespaces.unwrap(),
                whitespace_map: WhitespaceMap {
                    space: SharedString::new(whitespace_map.space.unwrap().to_string()),
                    tab: SharedString::new(whitespace_map.tab.unwrap().to_string()),
                },
                extend_comment_on_newline: settings.extend_comment_on_newline.unwrap(),
                inlay_hints: InlayHintSettings {
                    enabled: inlay_hints.enabled.unwrap(),
                    show_value_hints: inlay_hints.show_value_hints.unwrap(),
                    show_type_hints: inlay_hints.show_type_hints.unwrap(),
                    show_parameter_hints: inlay_hints.show_parameter_hints.unwrap(),
                    show_other_hints: inlay_hints.show_other_hints.unwrap(),
                    show_background: inlay_hints.show_background.unwrap(),
                    edit_debounce_ms: inlay_hints.edit_debounce_ms.unwrap(),
                    scroll_debounce_ms: inlay_hints.scroll_debounce_ms.unwrap(),
                    toggle_on_modifiers_press: inlay_hints.toggle_on_modifiers_press,
                },
                use_autoclose: settings.use_autoclose.unwrap(),
                use_auto_surround: settings.use_auto_surround.unwrap(),
                use_on_type_format: settings.use_on_type_format.unwrap(),
                auto_indent: settings.auto_indent.unwrap(),
                auto_indent_on_paste: settings.auto_indent_on_paste.unwrap(),
                always_treat_brackets_as_autoclosed: settings
                    .always_treat_brackets_as_autoclosed
                    .unwrap(),
                code_actions_on_format: settings.code_actions_on_format.unwrap(),
                linked_edits: settings.linked_edits.unwrap(),
                tasks: LanguageTaskSettings {
                    variables: tasks.variables,
                    enabled: tasks.enabled.unwrap(),
                    prefer_lsp: tasks.prefer_lsp.unwrap(),
                },
                show_completions_on_input: settings.show_completions_on_input.unwrap(),
                show_completion_documentation: settings.show_completion_documentation.unwrap(),
                completions: CompletionSettings {
                    words: completions.words.unwrap(),
                    words_min_length: completions.words_min_length.unwrap(),
                    lsp: completions.lsp.unwrap(),
                    lsp_fetch_timeout_ms: completions.lsp_fetch_timeout_ms.unwrap(),
                    lsp_insert_mode: completions.lsp_insert_mode.unwrap(),
                },
                debuggers: settings.debuggers.unwrap(),
            }
        }

        let default_language_settings = load_from_content(all_languages.defaults.clone());

        let mut languages = HashMap::default();
        for (language_name, settings) in &all_languages.languages.0 {
            let mut language_settings = all_languages.defaults.clone();
            settings::merge_from::MergeFrom::merge_from(&mut language_settings, settings);
            languages.insert(
                LanguageName(language_name.clone()),
                load_from_content(language_settings),
            );
        }

        let mut file_types: FxHashMap<Arc<str>, GlobSet> = FxHashMap::default();

        for (language, patterns) in &all_languages.file_types {
            let mut builder = GlobSetBuilder::new();

            for pattern in &patterns.0 {
                builder.add(Glob::new(pattern).unwrap());
            }

            file_types.insert(language.clone(), builder.build().unwrap());
        }

        Self {
            defaults: default_language_settings,
            languages,
            file_types,
        }
    }

    fn import_from_vscode(vscode: &settings::VsCodeSettings, current: &mut SettingsContent) {
        let d = &mut current.project.all_languages.defaults;
        if let Some(size) = vscode
            .read_value("editor.tabSize")
            .and_then(|v| v.as_u64())
            .and_then(|n| NonZeroU32::new(n as u32))
        {
            d.tab_size = Some(size);
        }
        if let Some(v) = vscode.read_bool("editor.insertSpaces") {
            d.hard_tabs = Some(!v);
        }

        vscode.enum_setting("editor.wordWrap", &mut d.soft_wrap, |s| match s {
            "on" => Some(SoftWrap::EditorWidth),
            "wordWrapColumn" => Some(SoftWrap::PreferLine),
            "bounded" => Some(SoftWrap::Bounded),
            "off" => Some(SoftWrap::None),
            _ => None,
        });
        vscode.u32_setting("editor.wordWrapColumn", &mut d.preferred_line_length);

        if let Some(arr) = vscode
            .read_value("editor.rulers")
            .and_then(|v| v.as_array())
            .map(|v| v.iter().map(|n| n.as_u64().map(|n| n as usize)).collect())
        {
            d.wrap_guides = arr;
        }
        if let Some(b) = vscode.read_bool("editor.guides.indentation") {
            d.indent_guides.get_or_insert_default().enabled = Some(b);
        }

        if let Some(b) = vscode.read_bool("editor.guides.formatOnSave") {
            d.format_on_save = Some(if b {
                FormatOnSave::On
            } else {
                FormatOnSave::Off
            });
        }
        vscode.bool_setting(
            "editor.trimAutoWhitespace",
            &mut d.remove_trailing_whitespace_on_save,
        );
        vscode.bool_setting(
            "files.insertFinalNewline",
            &mut d.ensure_final_newline_on_save,
        );
        vscode.enum_setting("editor.renderWhitespace", &mut d.show_whitespaces, |s| {
            Some(match s {
                "boundary" => ShowWhitespaceSetting::Boundary,
                "trailing" => ShowWhitespaceSetting::Trailing,
                "selection" => ShowWhitespaceSetting::Selection,
                "all" => ShowWhitespaceSetting::All,
                _ => ShowWhitespaceSetting::None,
            })
        });
        vscode.enum_setting(
            "editor.autoSurround",
            &mut d.use_auto_surround,
            |s| match s {
                "languageDefined" | "quotes" | "brackets" => Some(true),
                "never" => Some(false),
                _ => None,
            },
        );
        vscode.bool_setting("editor.formatOnType", &mut d.use_on_type_format);
        vscode.bool_setting("editor.linkedEditing", &mut d.linked_edits);
        vscode.bool_setting("editor.formatOnPaste", &mut d.auto_indent_on_paste);
        vscode.bool_setting(
            "editor.suggestOnTriggerCharacters",
            &mut d.show_completions_on_input,
        );
        if let Some(b) = vscode.read_bool("editor.suggest.showWords") {
            let mode = if b {
                WordsCompletionMode::Enabled
            } else {
                WordsCompletionMode::Disabled
            };
            d.completions.get_or_insert_default().words = Some(mode);
        }
        // TODO: pull ^ out into helper and reuse for per-language settings

        // vscodes file association map is inverted from ours, so we flip the mapping before merging
        let mut associations: HashMap<Arc<str>, ExtendingVec<String>> = HashMap::default();
        if let Some(map) = vscode
            .read_value("files.associations")
            .and_then(|v| v.as_object())
        {
            for (k, v) in map {
                let Some(v) = v.as_str() else { continue };
                associations.entry(v.into()).or_default().0.push(k.clone());
            }
        }

        // TODO: do we want to merge imported globs per filetype? for now we'll just replace
        current
            .project
            .all_languages
            .file_types
            .extend(associations);
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct JsxTagAutoCloseSettings {
    /// Enables or disables auto-closing of JSX tags.
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_language_servers() {
        fn language_server_names(names: &[&str]) -> Vec<LanguageServerName> {
            names
                .iter()
                .copied()
                .map(|name| LanguageServerName(name.to_string().into()))
                .collect::<Vec<_>>()
        }

        let available_language_servers = language_server_names(&[
            "typescript-language-server",
            "biome",
            "deno",
            "eslint",
            "tailwind",
        ]);

        // A value of just `["..."]` is the same as taking all of the available language servers.
        assert_eq!(
            LanguageSettings::resolve_language_servers(
                &[LanguageSettings::REST_OF_LANGUAGE_SERVERS.into()],
                &available_language_servers,
            ),
            available_language_servers
        );

        // Referencing one of the available language servers will change its order.
        assert_eq!(
            LanguageSettings::resolve_language_servers(
                &[
                    "biome".into(),
                    LanguageSettings::REST_OF_LANGUAGE_SERVERS.into(),
                    "deno".into()
                ],
                &available_language_servers
            ),
            language_server_names(&[
                "biome",
                "typescript-language-server",
                "eslint",
                "tailwind",
                "deno",
            ])
        );

        // Negating an available language server removes it from the list.
        assert_eq!(
            LanguageSettings::resolve_language_servers(
                &[
                    "deno".into(),
                    "!typescript-language-server".into(),
                    "!biome".into(),
                    LanguageSettings::REST_OF_LANGUAGE_SERVERS.into()
                ],
                &available_language_servers
            ),
            language_server_names(&["deno", "eslint", "tailwind"])
        );

        // Adding a language server not in the list of available language servers adds it to the list.
        assert_eq!(
            LanguageSettings::resolve_language_servers(
                &[
                    "my-cool-language-server".into(),
                    LanguageSettings::REST_OF_LANGUAGE_SERVERS.into()
                ],
                &available_language_servers
            ),
            language_server_names(&[
                "my-cool-language-server",
                "typescript-language-server",
                "biome",
                "deno",
                "eslint",
                "tailwind",
            ])
        );
    }
}
