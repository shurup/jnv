use std::cell::RefCell;

use anyhow::Result;

use jaq_interpret::{Ctx, FilterT, ParseCtx, RcIter, Val};

use promkit::{
    crossterm::{
        event::Event,
        style::{Attribute, Attributes, Color, ContentStyle},
    },
    json::{self, JsonNode, JsonPathSegment, JsonStream},
    listbox,
    pane::Pane,
    serde_json,
    snapshot::Snapshot,
    style::StyleBuilder,
    suggest::Suggest,
    switch::ActiveKeySwitcher,
    text, text_editor, PaneFactory, Prompt, PromptSignal,
};

use crate::trie::FilterTrie;

mod keymap;

fn run_jaq(
    query: &str,
    json_stream: Vec<serde_json::Value>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut ret = Vec::<serde_json::Value>::new();

    for input in json_stream {
        let mut ctx = ParseCtx::new(Vec::new());
        ctx.insert_natives(jaq_core::core());
        ctx.insert_defs(jaq_std::std());

        let (f, errs) = jaq_parse::parse(query, jaq_parse::main());
        if !errs.is_empty() {
            let error_message = errs
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(error_message));
        }

        let f = ctx.compile(f.unwrap());
        let inputs = RcIter::new(core::iter::empty());
        let mut out = f.run((Ctx::new([], &inputs), Val::from(input)));

        while let Some(Ok(val)) = out.next() {
            ret.push(val.into());
        }
    }

    Ok(ret)
}

pub struct Jnv {
    input_stream: Vec<serde_json::Value>,

    // Keybindings
    keymap: RefCell<ActiveKeySwitcher<keymap::Keymap>>,

    // For Rendering
    filter_editor: Snapshot<text_editor::State>,
    hint_message: Snapshot<text::State>,
    suggestions: listbox::State,
    json: json::State,

    // Store the filter history
    trie: FilterTrie,
    // Store the filter suggestions
    suggest: Suggest,

    json_expand_depth: Option<usize>,
    no_hint: bool,
}

impl Jnv {
    pub fn try_new(
        input_stream: Vec<serde_json::Value>,
        filter_editor: text_editor::State,
        hint_message: text::State,
        suggestions: listbox::State,
        json_theme: json::Theme,
        json_expand_depth: Option<usize>,
        no_hint: bool,
    ) -> Result<Prompt<Self>> {
        let mut trie = FilterTrie::default();
        trie.insert(".", input_stream.clone());

        let all_kinds = JsonStream::new(input_stream.clone(), None).flatten_kinds();
        let suggest = Suggest::from_iter(all_kinds.iter().filter_map(|kind| kind.path()).map(
            |segments| {
                if segments.is_empty() {
                    ".".to_string()
                } else {
                    segments
                        .iter()
                        .enumerate()
                        .map(|(i, segment)| match segment {
                            JsonPathSegment::Key(key) => {
                                if key.contains('.') || key.contains('-') || key.contains('@') {
                                    format!(".\"{}\"", key)
                                } else {
                                    format!(".{}", key)
                                }
                            }
                            JsonPathSegment::Index(index) => {
                                if i == 0 {
                                    format!(".[{}]", index)
                                } else {
                                    format!("[{}]", index)
                                }
                            }
                        })
                        .collect::<String>()
                }
            },
        ));

        Ok(Prompt {
            renderer: Self {
                keymap: RefCell::new(
                    ActiveKeySwitcher::new("default", self::keymap::default as keymap::Keymap)
                        .register("on_suggest", self::keymap::on_suggest),
                ),
                filter_editor: Snapshot::<text_editor::State>::new(filter_editor),
                hint_message: Snapshot::<text::State>::new(hint_message),
                suggestions,
                json: json::State {
                    stream: JsonStream::new(input_stream.clone(), json_expand_depth),
                    theme: json_theme,
                },
                trie,
                suggest,
                json_expand_depth,
                no_hint,
                input_stream,
            },
        })
    }

    fn update_hint_message(&mut self, text: String, style: ContentStyle) {
        if !self.no_hint {
            self.hint_message
                .after_mut()
                .replace(text::State { text, style })
        }
    }
}

impl promkit::Finalizer for Jnv {
    type Return = String;

    fn finalize(&self) -> anyhow::Result<Self::Return> {
        Ok(self
            .filter_editor
            .after()
            .texteditor
            .text_without_cursor()
            .to_string())
    }
}

impl promkit::Renderer for Jnv {
    fn create_panes(&self, width: u16, height: u16) -> Vec<Pane> {
        vec![
            self.filter_editor.create_pane(width, height),
            self.hint_message.create_pane(width, height),
            self.suggestions.create_pane(width, height),
            self.json.create_pane(width, height),
        ]
    }

    fn evaluate(&mut self, event: &Event) -> anyhow::Result<PromptSignal> {
        let keymap = *self.keymap.borrow_mut().get();
        let signal = keymap(event, self);
        let filter = self
            .filter_editor
            .after()
            .texteditor
            .text_without_cursor()
            .to_string();

        // Check if the query has changed
        if filter
            != self
                .filter_editor
                .borrow_before()
                .texteditor
                .text_without_cursor()
                .to_string()
        {
            self.hint_message.reset_after_to_init();

            match self.trie.exact_search(&filter) {
                Some(jsonl) => {
                    self.json.stream = JsonStream::new(jsonl.clone(), self.json_expand_depth);
                    self.update_hint_message(
                        format!(
                            "JSON query ('{}') was already executed. Result was retrieved from cache.",
                            &filter
                        ),
                        StyleBuilder::new()
                            .fgc(Color::DarkGrey)
                            .attrs(Attributes::from(Attribute::Bold))
                            .build(),
                    );
                }
                None => {
                    match run_jaq(&filter, self.input_stream.clone()) {
                        Ok(ret) => {
                            if ret.is_empty() {
                                self.update_hint_message(
                                    format!(
                                        "JSON query ('{}') was executed, but no results were returned.",
                                        &filter
                                    ),
                                    StyleBuilder::new()
                                        .fgc(Color::Red)
                                        .attrs(Attributes::from(Attribute::Bold))
                                        .build(),
                                );
                                if let Some(searched) = self.trie.prefix_search(&filter) {
                                    self.json.stream =
                                        JsonStream::new(searched.clone(), self.json_expand_depth);
                                }
                            } else {
                                let stream = JsonStream::new(ret.clone(), self.json_expand_depth);

                                let is_null = stream
                                    .roots()
                                    .iter()
                                    .all(|node| node == &JsonNode::Leaf(serde_json::Value::Null));
                                if is_null {
                                    self.update_hint_message(
                                        format!("JSON query resulted in 'null', which may indicate a typo or incorrect query: '{}'", &filter),
                                        StyleBuilder::new()
                                            .fgc(Color::Yellow)
                                            .attrs(Attributes::from(Attribute::Bold))
                                            .build(),
                                    );
                                    if let Some(searched) = self.trie.prefix_search(&filter) {
                                        self.json.stream = JsonStream::new(
                                            searched.clone(),
                                            self.json_expand_depth,
                                        );
                                    }
                                } else {
                                    // SUCCESS!
                                    self.trie.insert(&filter, ret);
                                    self.json.stream = stream;
                                }
                            }
                        }
                        Err(_) => {
                            self.update_hint_message(
                                format!("Failed to execute jq query '{}'", &filter),
                                StyleBuilder::new()
                                    .fgc(Color::Red)
                                    .attrs(Attributes::from(Attribute::Bold))
                                    .build(),
                            );
                            if let Some(searched) = self.trie.prefix_search(&filter) {
                                self.json.stream =
                                    JsonStream::new(searched.clone(), self.json_expand_depth);
                            }
                            return signal;
                        }
                    }
                }
            }
        }
        signal
    }
}
