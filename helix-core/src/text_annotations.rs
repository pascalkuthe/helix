use std::cell::Cell;
use std::convert::identity;
use std::ops::Range;

use crate::graphemes::GraphemeStr;
use crate::syntax::Highlight;

#[derive(Debug)]
pub struct InlineAnnotation {
    pub text: Box<str>,
    pub char_idx: usize,
}

#[derive(Debug)]
pub struct Overlay<'t> {
    pub char_idx: usize,
    pub grapheme: GraphemeStr<'t>,
}

#[derive(Debug)]
pub struct LineAnnotation {
    pub anchor_char_idx: usize,
    pub height: usize,
}

#[derive(Debug)]
struct Layer<'a, A, M> {
    annotations: &'a [A],
    current_index: Cell<usize>,
    metadata: M,
}

impl<'a, T, M: Clone> Clone for Layer<'a, T, M> {
    fn clone(&self) -> Self {
        Layer {
            annotations: self.annotations,
            current_index: self.current_index.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

impl<'a, T, M> Layer<'a, T, M> {
    pub fn reset_pos(&self, char_idx: usize, get_char_idx: impl Fn(&T) -> usize) {
        let new_index = self
            .annotations
            .binary_search_by_key(&char_idx, get_char_idx)
            .unwrap_or_else(identity);

        self.current_index.set(new_index);
    }

    pub fn consume(&self, char_idx: usize, get_char_idx: impl Fn(&T) -> usize) -> Option<&'a T> {
        let annot = self.annotations.get(self.current_index.get())?;
        debug_assert!(get_char_idx(annot) >= char_idx);
        if get_char_idx(annot) == char_idx {
            self.current_index.set(self.current_index.get() + 1);
            Some(annot)
        } else {
            None
        }
    }
}

impl<'a, T, M> From<(&'a [T], M)> for Layer<'a, T, M> {
    fn from((annotations, metadata): (&'a [T], M)) -> Layer<'a, T, M> {
        Layer {
            annotations,
            current_index: Cell::new(0),
            metadata,
        }
    }
}

fn reset_pos<T, M>(layers: &[Layer<T, M>], pos: usize, get_pos: impl Fn(&T) -> usize) {
    for layer in layers {
        layer.reset_pos(pos, &get_pos)
    }
}

/// Annotations that change that is displayed when the document is render.
/// Also commonly called virtual text.
#[derive(Default, Debug, Clone)]
pub struct TextAnnotations<'t> {
    inline_annotations: Vec<Layer<'t, InlineAnnotation, Option<Highlight>>>,
    overlays: Vec<Layer<'t, Overlay<'t>, Option<Highlight>>>,
    line_annotations: Vec<Layer<'t, LineAnnotation, ()>>,
}

impl<'t> TextAnnotations<'t> {
    pub fn reset_pos(&self, char_idx: usize) {
        reset_pos(&self.inline_annotations, char_idx, |annot| annot.char_idx);
        reset_pos(&self.overlays, char_idx, |annot| annot.char_idx);
        reset_pos(&self.line_annotations, char_idx, |annot| {
            annot.anchor_char_idx
        });
    }

    pub fn collect_overlay_highlights(
        &self,
        char_range: Range<usize>,
    ) -> Vec<(usize, Range<usize>)> {
        let mut highlights = Vec::new();
        for char_idx in char_range {
            if let Some((_, Some(highlight))) = self.overlay_at(char_idx) {
                // we don't know the number of chars the original grapheme takes
                // however it doesn't matter as highlight bounderies are automatically
                // aligned to grapheme boundaries in the rendering code
                highlights.push((highlight.0, char_idx..char_idx + 1))
            }
        }

        highlights
    }

    pub fn add_inline_annotations(
        &mut self,
        layer: &'t [InlineAnnotation],
        highlight: Option<Highlight>,
    ) -> &mut Self {
        self.inline_annotations.push((layer, highlight).into());
        self
    }

    pub fn add_overlay(
        &mut self,
        layer: &'t [Overlay<'t>],
        highlight: Option<Highlight>,
    ) -> &mut Self {
        self.overlays.push((layer, highlight).into());
        self
    }

    pub fn add_line_annotation(&mut self, layer: &'t [LineAnnotation]) -> &mut Self {
        self.line_annotations.push((layer, ()).into());
        self
    }

    pub fn clear_line_annotations(&mut self) {
        self.line_annotations.clear();
    }

    pub(crate) fn next_inline_annotation_at(
        &self,
        char_idx: usize,
    ) -> Option<(&'t InlineAnnotation, Option<Highlight>)> {
        self.inline_annotations.iter().find_map(|layer| {
            let annotation = layer.consume(char_idx, |annot| annot.char_idx)?;
            Some((annotation, layer.metadata))
        })
    }

    pub(crate) fn overlay_at(
        &self,
        char_idx: usize,
    ) -> Option<(&'t Overlay<'t>, Option<Highlight>)> {
        let mut overlay = None;
        for layer in &self.overlays {
            if let Some(new_overlay) = layer.consume(char_idx, |annot| annot.char_idx) {
                overlay = Some((new_overlay, layer.metadata))
            }
        }
        overlay
    }

    pub(crate) fn annotation_lines_at(&self, char_idx: usize) -> usize {
        self.line_annotations
            .iter()
            .map(|layer| {
                let mut lines = 0;
                while let Some(annot) = layer.annotations.get(layer.current_index.get()) {
                    if annot.anchor_char_idx == char_idx {
                        layer.current_index.set(layer.current_index.get() + 1);
                        lines += annot.height
                    } else {
                        break;
                    }
                }
                lines
            })
            .sum()
    }
}
