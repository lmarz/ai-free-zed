use crate::{ChunkRenderer, HighlightStyles, InlayId};
use collections::BTreeSet;
use gpui::{Hsla, Rgba};
use language::{Chunk, Edit, Point, TextSummary};
use multi_buffer::{
    Anchor, MultiBufferRow, MultiBufferRows, MultiBufferSnapshot, RowInfo, ToOffset,
};
use std::{
    cmp,
    ops::{Add, AddAssign, Range, Sub, SubAssign},
    sync::Arc,
};
use sum_tree::{Bias, Cursor, SumTree};
use text::{Patch, Rope};
use ui::{div, ActiveTheme, IntoElement as _, ParentElement as _, Styled as _};

use super::{custom_highlights::CustomHighlightsChunks, fold_map::ChunkRendererId, Highlights};

/// Decides where the [`Inlay`]s should be displayed.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct InlayMap {
    snapshot: InlaySnapshot,
    inlays: Vec<Inlay>,
}

#[derive(Clone)]
pub struct InlaySnapshot {
    pub buffer: MultiBufferSnapshot,
    transforms: SumTree<Transform>,
    pub version: usize,
}

#[derive(Clone, Debug)]
enum Transform {
    Isomorphic(TextSummary),
    Inlay(Inlay),
}

#[derive(Debug, Clone)]
pub struct Inlay {
    pub id: InlayId,
    pub position: Anchor,
    pub text: text::Rope,
    color: Option<Hsla>,
}

impl Inlay {
    pub fn hint(id: usize, position: Anchor, hint: &project::InlayHint) -> Self {
        let mut text = hint.text();
        if hint.padding_right && !text.ends_with(' ') {
            text.push(' ');
        }
        if hint.padding_left && !text.starts_with(' ') {
            text.insert(0, ' ');
        }
        Self {
            id: InlayId::Hint(id),
            position,
            text: text.into(),
            color: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn mock_hint(id: usize, position: Anchor, text: impl Into<Rope>) -> Self {
        Self {
            id: InlayId::Hint(id),
            position,
            text: text.into(),
            color: None,
        }
    }

    pub fn color(id: usize, position: Anchor, color: Rgba) -> Self {
        Self {
            id: InlayId::Color(id),
            position,
            text: Rope::from("◼"),
            color: Some(Hsla::from(color)),
        }
    }

    pub fn debugger<T: Into<Rope>>(id: usize, position: Anchor, text: T) -> Self {
        Self {
            id: InlayId::DebuggerValue(id),
            position,
            text: text.into(),
            color: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn get_color(&self) -> Option<Hsla> {
        self.color
    }
}

impl sum_tree::Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _: &()) -> Self::Summary {
        match self {
            Transform::Isomorphic(summary) => TransformSummary {
                input: *summary,
                output: *summary,
            },
            Transform::Inlay(inlay) => TransformSummary {
                input: TextSummary::default(),
                output: inlay.text.summary(),
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
struct TransformSummary {
    input: TextSummary,
    output: TextSummary,
}

impl sum_tree::Summary for TransformSummary {
    type Context = ();

    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, other: &Self, _: &()) {
        self.input += &other.input;
        self.output += &other.output;
    }
}

pub type InlayEdit = Edit<InlayOffset>;

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct InlayOffset(pub usize);

impl Add for InlayOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for InlayOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for InlayOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl SubAssign for InlayOffset {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayOffset {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: &()) {
        self.0 += &summary.output.len;
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct InlayPoint(pub Point);

impl Add for InlayPoint {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for InlayPoint {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for InlayPoint {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: &()) {
        self.0 += &summary.output.lines;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for usize {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: &()) {
        *self += &summary.input.len;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for Point {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: &()) {
        *self += &summary.input.lines;
    }
}

#[derive(Clone)]
pub struct InlayBufferRows<'a> {
    transforms: Cursor<'a, Transform, (InlayPoint, Point)>,
    buffer_rows: MultiBufferRows<'a>,
    inlay_row: u32,
    max_buffer_row: MultiBufferRow,
}

pub struct InlayChunks<'a> {
    transforms: Cursor<'a, Transform, (InlayOffset, usize)>,
    buffer_chunks: CustomHighlightsChunks<'a>,
    buffer_chunk: Option<Chunk<'a>>,
    inlay_chunks: Option<text::Chunks<'a>>,
    inlay_chunk: Option<&'a str>,
    output_offset: InlayOffset,
    max_output_offset: InlayOffset,
    highlight_styles: HighlightStyles,
    highlights: Highlights<'a>,
    snapshot: &'a InlaySnapshot,
}

#[derive(Clone)]
pub struct InlayChunk<'a> {
    pub chunk: Chunk<'a>,
    /// Whether the inlay should be customly rendered.
    pub renderer: Option<ChunkRenderer>,
}

impl InlayChunks<'_> {
    pub fn seek(&mut self, new_range: Range<InlayOffset>) {
        self.transforms.seek(&new_range.start, Bias::Right, &());

        let buffer_range = self.snapshot.to_buffer_offset(new_range.start)
            ..self.snapshot.to_buffer_offset(new_range.end);
        self.buffer_chunks.seek(buffer_range);
        self.inlay_chunks = None;
        self.buffer_chunk = None;
        self.output_offset = new_range.start;
        self.max_output_offset = new_range.end;
    }

    pub fn offset(&self) -> InlayOffset {
        self.output_offset
    }
}

impl<'a> Iterator for InlayChunks<'a> {
    type Item = InlayChunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.output_offset == self.max_output_offset {
            return None;
        }

        let chunk = match self.transforms.item()? {
            Transform::Isomorphic(_) => {
                let chunk = self
                    .buffer_chunk
                    .get_or_insert_with(|| self.buffer_chunks.next().unwrap());
                if chunk.text.is_empty() {
                    *chunk = self.buffer_chunks.next().unwrap();
                }

                let (prefix, suffix) = chunk.text.split_at(
                    chunk
                        .text
                        .len()
                        .min(self.transforms.end(&()).0 .0 - self.output_offset.0),
                );

                chunk.text = suffix;
                self.output_offset.0 += prefix.len();
                InlayChunk {
                    chunk: Chunk {
                        text: prefix,
                        ..chunk.clone()
                    },
                    renderer: None,
                }
            }
            Transform::Inlay(inlay) => {
                let mut inlay_style_and_highlight = None;
                if let Some(inlay_highlights) = self.highlights.inlay_highlights {
                    for (_, inlay_id_to_data) in inlay_highlights.iter() {
                        let style_and_highlight = inlay_id_to_data.get(&inlay.id);
                        if style_and_highlight.is_some() {
                            inlay_style_and_highlight = style_and_highlight;
                            break;
                        }
                    }
                }

                let mut renderer = None;
                let mut highlight_style = match inlay.id {
                    InlayId::Hint(_) => self.highlight_styles.inlay_hint,
                    InlayId::DebuggerValue(_) => self.highlight_styles.inlay_hint,
                    InlayId::Color(_) => {
                        if let Some(color) = inlay.color {
                            renderer = Some(ChunkRenderer {
                                id: ChunkRendererId::Inlay(inlay.id),
                                render: Arc::new(move |cx| {
                                    div()
                                        .w_4()
                                        .h_4()
                                        .relative()
                                        .child(
                                            div()
                                                .absolute()
                                                .right_1()
                                                .w_3p5()
                                                .h_3p5()
                                                .border_2()
                                                .border_color(cx.theme().colors().border)
                                                .bg(color),
                                        )
                                        .into_any_element()
                                }),
                                constrain_width: false,
                                measured_width: None,
                            });
                        }
                        self.highlight_styles.inlay_hint
                    }
                };
                let next_inlay_highlight_endpoint;
                let offset_in_inlay = self.output_offset - self.transforms.start().0;
                if let Some((style, highlight)) = inlay_style_and_highlight {
                    let range = &highlight.range;
                    if offset_in_inlay.0 < range.start {
                        next_inlay_highlight_endpoint = range.start - offset_in_inlay.0;
                    } else if offset_in_inlay.0 >= range.end {
                        next_inlay_highlight_endpoint = usize::MAX;
                    } else {
                        next_inlay_highlight_endpoint = range.end - offset_in_inlay.0;
                        highlight_style
                            .get_or_insert_with(Default::default)
                            .highlight(*style);
                    }
                } else {
                    next_inlay_highlight_endpoint = usize::MAX;
                }

                let inlay_chunks = self.inlay_chunks.get_or_insert_with(|| {
                    let start = offset_in_inlay;
                    let end = cmp::min(self.max_output_offset, self.transforms.end(&()).0)
                        - self.transforms.start().0;
                    inlay.text.chunks_in_range(start.0..end.0)
                });
                let inlay_chunk = self
                    .inlay_chunk
                    .get_or_insert_with(|| inlay_chunks.next().unwrap());
                let (chunk, remainder) =
                    inlay_chunk.split_at(inlay_chunk.len().min(next_inlay_highlight_endpoint));
                *inlay_chunk = remainder;
                if inlay_chunk.is_empty() {
                    self.inlay_chunk = None;
                }

                self.output_offset.0 += chunk.len();

                InlayChunk {
                    chunk: Chunk {
                        text: chunk,
                        highlight_style,
                        is_inlay: true,
                        ..Chunk::default()
                    },
                    renderer,
                }
            }
        };

        if self.output_offset == self.transforms.end(&()).0 {
            self.inlay_chunks = None;
            self.transforms.next(&());
        }

        Some(chunk)
    }
}

impl InlayBufferRows<'_> {
    pub fn seek(&mut self, row: u32) {
        let inlay_point = InlayPoint::new(row, 0);
        self.transforms.seek(&inlay_point, Bias::Left, &());

        let mut buffer_point = self.transforms.start().1;
        let buffer_row = MultiBufferRow(if row == 0 {
            0
        } else {
            match self.transforms.item() {
                Some(Transform::Isomorphic(_)) => {
                    buffer_point += inlay_point.0 - self.transforms.start().0 .0;
                    buffer_point.row
                }
                _ => cmp::min(buffer_point.row + 1, self.max_buffer_row.0),
            }
        });
        self.inlay_row = inlay_point.row();
        self.buffer_rows.seek(buffer_row);
    }
}

impl Iterator for InlayBufferRows<'_> {
    type Item = RowInfo;

    fn next(&mut self) -> Option<Self::Item> {
        let buffer_row = if self.inlay_row == 0 {
            self.buffer_rows.next().unwrap()
        } else {
            match self.transforms.item()? {
                Transform::Inlay(_) => Default::default(),
                Transform::Isomorphic(_) => self.buffer_rows.next().unwrap(),
            }
        };

        self.inlay_row += 1;
        self.transforms
            .seek_forward(&InlayPoint::new(self.inlay_row, 0), Bias::Left, &());

        Some(buffer_row)
    }
}

impl InlayPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(Point::new(row, column))
    }

    pub fn row(self) -> u32 {
        self.0.row
    }
}

impl InlayMap {
    pub fn new(buffer: MultiBufferSnapshot) -> (Self, InlaySnapshot) {
        let version = 0;
        let snapshot = InlaySnapshot {
            buffer: buffer.clone(),
            transforms: SumTree::from_iter(Some(Transform::Isomorphic(buffer.text_summary())), &()),
            version,
        };

        (
            Self {
                snapshot: snapshot.clone(),
                inlays: Vec::new(),
            },
            snapshot,
        )
    }

    pub fn sync(
        &mut self,
        buffer_snapshot: MultiBufferSnapshot,
        mut buffer_edits: Vec<text::Edit<usize>>,
    ) -> (InlaySnapshot, Vec<InlayEdit>) {
        let snapshot = &mut self.snapshot;

        if buffer_edits.is_empty()
            && snapshot.buffer.trailing_excerpt_update_count()
                != buffer_snapshot.trailing_excerpt_update_count()
        {
            buffer_edits.push(Edit {
                old: snapshot.buffer.len()..snapshot.buffer.len(),
                new: buffer_snapshot.len()..buffer_snapshot.len(),
            });
        }

        if buffer_edits.is_empty() {
            if snapshot.buffer.edit_count() != buffer_snapshot.edit_count()
                || snapshot.buffer.non_text_state_update_count()
                    != buffer_snapshot.non_text_state_update_count()
                || snapshot.buffer.trailing_excerpt_update_count()
                    != buffer_snapshot.trailing_excerpt_update_count()
            {
                snapshot.version += 1;
            }

            snapshot.buffer = buffer_snapshot;
            (snapshot.clone(), Vec::new())
        } else {
            let mut inlay_edits = Patch::default();
            let mut new_transforms = SumTree::default();
            let mut cursor = snapshot.transforms.cursor::<(usize, InlayOffset)>(&());
            let mut buffer_edits_iter = buffer_edits.iter().peekable();
            while let Some(buffer_edit) = buffer_edits_iter.next() {
                new_transforms.append(cursor.slice(&buffer_edit.old.start, Bias::Left, &()), &());
                if let Some(Transform::Isomorphic(transform)) = cursor.item() {
                    if cursor.end(&()).0 == buffer_edit.old.start {
                        push_isomorphic(&mut new_transforms, *transform);
                        cursor.next(&());
                    }
                }

                // Remove all the inlays and transforms contained by the edit.
                let old_start =
                    cursor.start().1 + InlayOffset(buffer_edit.old.start - cursor.start().0);
                cursor.seek(&buffer_edit.old.end, Bias::Right, &());
                let old_end =
                    cursor.start().1 + InlayOffset(buffer_edit.old.end - cursor.start().0);

                // Push the unchanged prefix.
                let prefix_start = new_transforms.summary().input.len;
                let prefix_end = buffer_edit.new.start;
                push_isomorphic(
                    &mut new_transforms,
                    buffer_snapshot.text_summary_for_range(prefix_start..prefix_end),
                );
                let new_start = InlayOffset(new_transforms.summary().output.len);

                let start_ix = match self.inlays.binary_search_by(|probe| {
                    probe
                        .position
                        .to_offset(&buffer_snapshot)
                        .cmp(&buffer_edit.new.start)
                        .then(std::cmp::Ordering::Greater)
                }) {
                    Ok(ix) | Err(ix) => ix,
                };

                for inlay in &self.inlays[start_ix..] {
                    if !inlay.position.is_valid(&buffer_snapshot) {
                        continue;
                    }
                    let buffer_offset = inlay.position.to_offset(&buffer_snapshot);
                    if buffer_offset > buffer_edit.new.end {
                        break;
                    }

                    let prefix_start = new_transforms.summary().input.len;
                    let prefix_end = buffer_offset;
                    push_isomorphic(
                        &mut new_transforms,
                        buffer_snapshot.text_summary_for_range(prefix_start..prefix_end),
                    );

                    new_transforms.push(Transform::Inlay(inlay.clone()), &());
                }

                // Apply the rest of the edit.
                let transform_start = new_transforms.summary().input.len;
                push_isomorphic(
                    &mut new_transforms,
                    buffer_snapshot.text_summary_for_range(transform_start..buffer_edit.new.end),
                );
                let new_end = InlayOffset(new_transforms.summary().output.len);
                inlay_edits.push(Edit {
                    old: old_start..old_end,
                    new: new_start..new_end,
                });

                // If the next edit doesn't intersect the current isomorphic transform, then
                // we can push its remainder.
                if buffer_edits_iter
                    .peek()
                    .map_or(true, |edit| edit.old.start >= cursor.end(&()).0)
                {
                    let transform_start = new_transforms.summary().input.len;
                    let transform_end =
                        buffer_edit.new.end + (cursor.end(&()).0 - buffer_edit.old.end);
                    push_isomorphic(
                        &mut new_transforms,
                        buffer_snapshot.text_summary_for_range(transform_start..transform_end),
                    );
                    cursor.next(&());
                }
            }

            new_transforms.append(cursor.suffix(&()), &());
            if new_transforms.is_empty() {
                new_transforms.push(Transform::Isomorphic(Default::default()), &());
            }

            drop(cursor);
            snapshot.transforms = new_transforms;
            snapshot.version += 1;
            snapshot.buffer = buffer_snapshot;
            snapshot.check_invariants();

            (snapshot.clone(), inlay_edits.into_inner())
        }
    }

    pub fn splice(
        &mut self,
        to_remove: &[InlayId],
        to_insert: Vec<Inlay>,
    ) -> (InlaySnapshot, Vec<InlayEdit>) {
        let snapshot = &mut self.snapshot;
        let mut edits = BTreeSet::new();

        self.inlays.retain(|inlay| {
            let retain = !to_remove.contains(&inlay.id);
            if !retain {
                let offset = inlay.position.to_offset(&snapshot.buffer);
                edits.insert(offset);
            }
            retain
        });

        for inlay_to_insert in to_insert {
            // Avoid inserting empty inlays.
            if inlay_to_insert.text.is_empty() {
                continue;
            }

            let offset = inlay_to_insert.position.to_offset(&snapshot.buffer);
            match self.inlays.binary_search_by(|probe| {
                probe
                    .position
                    .cmp(&inlay_to_insert.position, &snapshot.buffer)
                    .then(std::cmp::Ordering::Less)
            }) {
                Ok(ix) | Err(ix) => {
                    self.inlays.insert(ix, inlay_to_insert);
                }
            }

            edits.insert(offset);
        }

        let buffer_edits = edits
            .into_iter()
            .map(|offset| Edit {
                old: offset..offset,
                new: offset..offset,
            })
            .collect();
        let buffer_snapshot = snapshot.buffer.clone();
        let (snapshot, edits) = self.sync(buffer_snapshot, buffer_edits);
        (snapshot, edits)
    }

    pub fn current_inlays(&self) -> impl Iterator<Item = &Inlay> {
        self.inlays.iter()
    }
}

impl InlaySnapshot {
    pub fn to_point(&self, offset: InlayOffset) -> InlayPoint {
        let mut cursor = self
            .transforms
            .cursor::<(InlayOffset, (InlayPoint, usize))>(&());
        cursor.seek(&offset, Bias::Right, &());
        let overshoot = offset.0 - cursor.start().0 .0;
        match cursor.item() {
            Some(Transform::Isomorphic(_)) => {
                let buffer_offset_start = cursor.start().1 .1;
                let buffer_offset_end = buffer_offset_start + overshoot;
                let buffer_start = self.buffer.offset_to_point(buffer_offset_start);
                let buffer_end = self.buffer.offset_to_point(buffer_offset_end);
                InlayPoint(cursor.start().1 .0 .0 + (buffer_end - buffer_start))
            }
            Some(Transform::Inlay(inlay)) => {
                let overshoot = inlay.text.offset_to_point(overshoot);
                InlayPoint(cursor.start().1 .0 .0 + overshoot)
            }
            None => self.max_point(),
        }
    }

    pub fn len(&self) -> InlayOffset {
        InlayOffset(self.transforms.summary().output.len)
    }

    pub fn max_point(&self) -> InlayPoint {
        InlayPoint(self.transforms.summary().output.lines)
    }

    pub fn to_offset(&self, point: InlayPoint) -> InlayOffset {
        let mut cursor = self
            .transforms
            .cursor::<(InlayPoint, (InlayOffset, Point))>(&());
        cursor.seek(&point, Bias::Right, &());
        let overshoot = point.0 - cursor.start().0 .0;
        match cursor.item() {
            Some(Transform::Isomorphic(_)) => {
                let buffer_point_start = cursor.start().1 .1;
                let buffer_point_end = buffer_point_start + overshoot;
                let buffer_offset_start = self.buffer.point_to_offset(buffer_point_start);
                let buffer_offset_end = self.buffer.point_to_offset(buffer_point_end);
                InlayOffset(cursor.start().1 .0 .0 + (buffer_offset_end - buffer_offset_start))
            }
            Some(Transform::Inlay(inlay)) => {
                let overshoot = inlay.text.point_to_offset(overshoot);
                InlayOffset(cursor.start().1 .0 .0 + overshoot)
            }
            None => self.len(),
        }
    }
    pub fn to_buffer_point(&self, point: InlayPoint) -> Point {
        let mut cursor = self.transforms.cursor::<(InlayPoint, Point)>(&());
        cursor.seek(&point, Bias::Right, &());
        match cursor.item() {
            Some(Transform::Isomorphic(_)) => {
                let overshoot = point.0 - cursor.start().0 .0;
                cursor.start().1 + overshoot
            }
            Some(Transform::Inlay(_)) => cursor.start().1,
            None => self.buffer.max_point(),
        }
    }
    pub fn to_buffer_offset(&self, offset: InlayOffset) -> usize {
        let mut cursor = self.transforms.cursor::<(InlayOffset, usize)>(&());
        cursor.seek(&offset, Bias::Right, &());
        match cursor.item() {
            Some(Transform::Isomorphic(_)) => {
                let overshoot = offset - cursor.start().0;
                cursor.start().1 + overshoot.0
            }
            Some(Transform::Inlay(_)) => cursor.start().1,
            None => self.buffer.len(),
        }
    }

    pub fn to_inlay_offset(&self, offset: usize) -> InlayOffset {
        let mut cursor = self.transforms.cursor::<(usize, InlayOffset)>(&());
        cursor.seek(&offset, Bias::Left, &());
        loop {
            match cursor.item() {
                Some(Transform::Isomorphic(_)) => {
                    if offset == cursor.end(&()).0 {
                        while let Some(Transform::Inlay(inlay)) = cursor.next_item() {
                            if inlay.position.bias() == Bias::Right {
                                break;
                            } else {
                                cursor.next(&());
                            }
                        }
                        return cursor.end(&()).1;
                    } else {
                        let overshoot = offset - cursor.start().0;
                        return InlayOffset(cursor.start().1 .0 + overshoot);
                    }
                }
                Some(Transform::Inlay(inlay)) => {
                    if inlay.position.bias() == Bias::Left {
                        cursor.next(&());
                    } else {
                        return cursor.start().1;
                    }
                }
                None => {
                    return self.len();
                }
            }
        }
    }
    pub fn to_inlay_point(&self, point: Point) -> InlayPoint {
        let mut cursor = self.transforms.cursor::<(Point, InlayPoint)>(&());
        cursor.seek(&point, Bias::Left, &());
        loop {
            match cursor.item() {
                Some(Transform::Isomorphic(_)) => {
                    if point == cursor.end(&()).0 {
                        while let Some(Transform::Inlay(inlay)) = cursor.next_item() {
                            if inlay.position.bias() == Bias::Right {
                                break;
                            } else {
                                cursor.next(&());
                            }
                        }
                        return cursor.end(&()).1;
                    } else {
                        let overshoot = point - cursor.start().0;
                        return InlayPoint(cursor.start().1 .0 + overshoot);
                    }
                }
                Some(Transform::Inlay(inlay)) => {
                    if inlay.position.bias() == Bias::Left {
                        cursor.next(&());
                    } else {
                        return cursor.start().1;
                    }
                }
                None => {
                    return self.max_point();
                }
            }
        }
    }

    pub fn clip_point(&self, mut point: InlayPoint, mut bias: Bias) -> InlayPoint {
        let mut cursor = self.transforms.cursor::<(InlayPoint, Point)>(&());
        cursor.seek(&point, Bias::Left, &());
        loop {
            match cursor.item() {
                Some(Transform::Isomorphic(transform)) => {
                    if cursor.start().0 == point {
                        if let Some(Transform::Inlay(inlay)) = cursor.prev_item() {
                            if inlay.position.bias() == Bias::Left {
                                return point;
                            } else if bias == Bias::Left {
                                cursor.prev(&());
                            } else if transform.first_line_chars == 0 {
                                point.0 += Point::new(1, 0);
                            } else {
                                point.0 += Point::new(0, 1);
                            }
                        } else {
                            return point;
                        }
                    } else if cursor.end(&()).0 == point {
                        if let Some(Transform::Inlay(inlay)) = cursor.next_item() {
                            if inlay.position.bias() == Bias::Right {
                                return point;
                            } else if bias == Bias::Right {
                                cursor.next(&());
                            } else if point.0.column == 0 {
                                point.0.row -= 1;
                                point.0.column = self.line_len(point.0.row);
                            } else {
                                point.0.column -= 1;
                            }
                        } else {
                            return point;
                        }
                    } else {
                        let overshoot = point.0 - cursor.start().0 .0;
                        let buffer_point = cursor.start().1 + overshoot;
                        let clipped_buffer_point = self.buffer.clip_point(buffer_point, bias);
                        let clipped_overshoot = clipped_buffer_point - cursor.start().1;
                        let clipped_point = InlayPoint(cursor.start().0 .0 + clipped_overshoot);
                        if clipped_point == point {
                            return clipped_point;
                        } else {
                            point = clipped_point;
                        }
                    }
                }
                Some(Transform::Inlay(inlay)) => {
                    if point == cursor.start().0 && inlay.position.bias() == Bias::Right {
                        match cursor.prev_item() {
                            Some(Transform::Inlay(inlay)) => {
                                if inlay.position.bias() == Bias::Left {
                                    return point;
                                }
                            }
                            _ => return point,
                        }
                    } else if point == cursor.end(&()).0 && inlay.position.bias() == Bias::Left {
                        match cursor.next_item() {
                            Some(Transform::Inlay(inlay)) => {
                                if inlay.position.bias() == Bias::Right {
                                    return point;
                                }
                            }
                            _ => return point,
                        }
                    }

                    if bias == Bias::Left {
                        point = cursor.start().0;
                        cursor.prev(&());
                    } else {
                        cursor.next(&());
                        point = cursor.start().0;
                    }
                }
                None => {
                    bias = bias.invert();
                    if bias == Bias::Left {
                        point = cursor.start().0;
                        cursor.prev(&());
                    } else {
                        cursor.next(&());
                        point = cursor.start().0;
                    }
                }
            }
        }
    }

    pub fn text_summary(&self) -> TextSummary {
        self.transforms.summary().output
    }

    pub fn text_summary_for_range(&self, range: Range<InlayOffset>) -> TextSummary {
        let mut summary = TextSummary::default();

        let mut cursor = self.transforms.cursor::<(InlayOffset, usize)>(&());
        cursor.seek(&range.start, Bias::Right, &());

        let overshoot = range.start.0 - cursor.start().0 .0;
        match cursor.item() {
            Some(Transform::Isomorphic(_)) => {
                let buffer_start = cursor.start().1;
                let suffix_start = buffer_start + overshoot;
                let suffix_end =
                    buffer_start + (cmp::min(cursor.end(&()).0, range.end).0 - cursor.start().0 .0);
                summary = self.buffer.text_summary_for_range(suffix_start..suffix_end);
                cursor.next(&());
            }
            Some(Transform::Inlay(inlay)) => {
                let suffix_start = overshoot;
                let suffix_end = cmp::min(cursor.end(&()).0, range.end).0 - cursor.start().0 .0;
                summary = inlay.text.cursor(suffix_start).summary(suffix_end);
                cursor.next(&());
            }
            None => {}
        }

        if range.end > cursor.start().0 {
            summary += cursor
                .summary::<_, TransformSummary>(&range.end, Bias::Right, &())
                .output;

            let overshoot = range.end.0 - cursor.start().0 .0;
            match cursor.item() {
                Some(Transform::Isomorphic(_)) => {
                    let prefix_start = cursor.start().1;
                    let prefix_end = prefix_start + overshoot;
                    summary += self
                        .buffer
                        .text_summary_for_range::<TextSummary, _>(prefix_start..prefix_end);
                }
                Some(Transform::Inlay(inlay)) => {
                    let prefix_end = overshoot;
                    summary += inlay.text.cursor(0).summary::<TextSummary>(prefix_end);
                }
                None => {}
            }
        }

        summary
    }

    pub fn row_infos(&self, row: u32) -> InlayBufferRows<'_> {
        let mut cursor = self.transforms.cursor::<(InlayPoint, Point)>(&());
        let inlay_point = InlayPoint::new(row, 0);
        cursor.seek(&inlay_point, Bias::Left, &());

        let max_buffer_row = self.buffer.max_row();
        let mut buffer_point = cursor.start().1;
        let buffer_row = if row == 0 {
            MultiBufferRow(0)
        } else {
            match cursor.item() {
                Some(Transform::Isomorphic(_)) => {
                    buffer_point += inlay_point.0 - cursor.start().0 .0;
                    MultiBufferRow(buffer_point.row)
                }
                _ => cmp::min(MultiBufferRow(buffer_point.row + 1), max_buffer_row),
            }
        };

        InlayBufferRows {
            transforms: cursor,
            inlay_row: inlay_point.row(),
            buffer_rows: self.buffer.row_infos(buffer_row),
            max_buffer_row,
        }
    }

    pub fn line_len(&self, row: u32) -> u32 {
        let line_start = self.to_offset(InlayPoint::new(row, 0)).0;
        let line_end = if row >= self.max_point().row() {
            self.len().0
        } else {
            self.to_offset(InlayPoint::new(row + 1, 0)).0 - 1
        };
        (line_end - line_start) as u32
    }

    pub(crate) fn chunks<'a>(
        &'a self,
        range: Range<InlayOffset>,
        language_aware: bool,
        highlights: Highlights<'a>,
    ) -> InlayChunks<'a> {
        let mut cursor = self.transforms.cursor::<(InlayOffset, usize)>(&());
        cursor.seek(&range.start, Bias::Right, &());

        let buffer_range = self.to_buffer_offset(range.start)..self.to_buffer_offset(range.end);
        let buffer_chunks = CustomHighlightsChunks::new(
            buffer_range,
            language_aware,
            highlights.text_highlights,
            &self.buffer,
        );

        InlayChunks {
            transforms: cursor,
            buffer_chunks,
            inlay_chunks: None,
            inlay_chunk: None,
            buffer_chunk: None,
            output_offset: range.start,
            max_output_offset: range.end,
            highlight_styles: highlights.styles,
            highlights,
            snapshot: self,
        }
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.chunks(Default::default()..self.len(), false, Highlights::default())
            .map(|chunk| chunk.chunk.text)
            .collect()
    }

    fn check_invariants(&self) {
        #[cfg(any(debug_assertions, feature = "test-support"))]
        {
            assert_eq!(self.transforms.summary().input, self.buffer.text_summary());
            let mut transforms = self.transforms.iter().peekable();
            while let Some(transform) = transforms.next() {
                let transform_is_isomorphic = matches!(transform, Transform::Isomorphic(_));
                if let Some(next_transform) = transforms.peek() {
                    let next_transform_is_isomorphic =
                        matches!(next_transform, Transform::Isomorphic(_));
                    assert!(
                        !transform_is_isomorphic || !next_transform_is_isomorphic,
                        "two adjacent isomorphic transforms"
                    );
                }
            }
        }
    }
}

fn push_isomorphic(sum_tree: &mut SumTree<Transform>, summary: TextSummary) {
    if summary.len == 0 {
        return;
    }

    let mut summary = Some(summary);
    sum_tree.update_last(
        |transform| {
            if let Transform::Isomorphic(transform) = transform {
                *transform += summary.take().unwrap();
            }
        },
        &(),
    );

    if let Some(summary) = summary {
        sum_tree.push(Transform::Isomorphic(summary), &());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use project::{InlayHint, InlayHintLabel, ResolveState};

    #[test]
    fn test_inlay_properties_label_padding() {
        assert_eq!(
            Inlay::hint(
                0,
                Anchor::min(),
                &InlayHint {
                    label: InlayHintLabel::String("a".to_string()),
                    position: text::Anchor::default(),
                    padding_left: false,
                    padding_right: false,
                    tooltip: None,
                    kind: None,
                    resolve_state: ResolveState::Resolved,
                },
            )
            .text
            .to_string(),
            "a",
            "Should not pad label if not requested"
        );

        assert_eq!(
            Inlay::hint(
                0,
                Anchor::min(),
                &InlayHint {
                    label: InlayHintLabel::String("a".to_string()),
                    position: text::Anchor::default(),
                    padding_left: true,
                    padding_right: true,
                    tooltip: None,
                    kind: None,
                    resolve_state: ResolveState::Resolved,
                },
            )
            .text
            .to_string(),
            " a ",
            "Should pad label for every side requested"
        );

        assert_eq!(
            Inlay::hint(
                0,
                Anchor::min(),
                &InlayHint {
                    label: InlayHintLabel::String(" a ".to_string()),
                    position: text::Anchor::default(),
                    padding_left: false,
                    padding_right: false,
                    tooltip: None,
                    kind: None,
                    resolve_state: ResolveState::Resolved,
                },
            )
            .text
            .to_string(),
            " a ",
            "Should not change already padded label"
        );

        assert_eq!(
            Inlay::hint(
                0,
                Anchor::min(),
                &InlayHint {
                    label: InlayHintLabel::String(" a ".to_string()),
                    position: text::Anchor::default(),
                    padding_left: true,
                    padding_right: true,
                    tooltip: None,
                    kind: None,
                    resolve_state: ResolveState::Resolved,
                },
            )
            .text
            .to_string(),
            " a ",
            "Should not change already padded label"
        );
    }
}
