use std::ops::Range;

use helix_core::{graphemes::prev_grapheme_boundary, Rope};

use crate::{document::review_comments::CommentSide, Document};

use super::diff::Deletion;

#[derive(Clone)]
pub enum ReviewBlockContent {
    Deleted {
        deletion: usize,
        before: Range<u32>,
    },
    Comment {
        id: u64,
        text: Rope,
        side: CommentSide,
        range: Range<usize>,
    },
}

pub struct ReviewBlock {
    pub anchor: usize,
    pub at_start: bool,
    /// Offset within all virtual blocks attached to this source anchor.
    pub offset: usize,
    pub content: ReviewBlockContent,
}

impl ReviewBlock {
    pub fn height(&self) -> usize {
        match &self.content {
            ReviewBlockContent::Deleted { before, .. } => before.len(),
            ReviewBlockContent::Comment { text, .. } => text.len_lines(),
        }
    }
}

pub(super) fn review_blocks(
    doc: &Document,
    base: &Rope,
    deletions: &[Deletion],
) -> Vec<ReviewBlock> {
    let mut blocks = Vec::new();
    let reference = doc.review_diff_reference().unwrap_or("HEAD");
    let located: Vec<_> = doc
        .review_comments()
        .iter()
        .filter(|comment| doc.review_comment_visible(comment.id))
        .filter_map(|comment| {
            let text = match comment.anchor.side {
                CommentSide::Current => doc.text(),
                CommentSide::Base if comment.anchor.reference.as_deref() == Some(reference) => base,
                _ => return None,
            };
            Some((comment, comment.anchor.locate(text.slice(..))?))
        })
        .collect();

    for (index, deletion) in deletions.iter().enumerate() {
        let mut comments: Vec<_> = located
            .iter()
            .filter(|(comment, range)| {
                comment.anchor.side == CommentSide::Base
                    && deletion
                        .before
                        .contains(&(base.char_to_line(range.start) as u32))
            })
            .collect();
        comments.sort_by_key(|(comment, range)| (base.char_to_line(range.start), comment.id));
        let mut first = deletion.before.start;
        for (comment, range) in comments {
            let line = base.char_to_line(range.start) as u32;
            if first < line {
                blocks.push(ReviewBlock {
                    anchor: deletion.anchor,
                    at_start: deletion.at_start,
                    offset: 0,
                    content: ReviewBlockContent::Deleted {
                        deletion: index,
                        before: first..line,
                    },
                });
            }
            blocks.push(ReviewBlock {
                anchor: deletion.anchor,
                at_start: deletion.at_start,
                offset: 0,
                content: ReviewBlockContent::Comment {
                    id: comment.id,
                    text: Rope::from_str(&comment.text),
                    side: CommentSide::Base,
                    range: range.clone(),
                },
            });
            first = line;
        }
        if first < deletion.before.end {
            blocks.push(ReviewBlock {
                anchor: deletion.anchor,
                at_start: deletion.at_start,
                offset: 0,
                content: ReviewBlockContent::Deleted {
                    deletion: index,
                    before: first..deletion.before.end,
                },
            });
        }
    }
    for (comment, range) in located {
        if comment.anchor.side != CommentSide::Current {
            continue;
        }
        let line_start = doc
            .text()
            .line_to_char(doc.text().char_to_line(range.start));
        blocks.push(ReviewBlock {
            anchor: if line_start == 0 {
                0
            } else {
                prev_grapheme_boundary(doc.text().slice(..), line_start)
            },
            at_start: line_start == 0,
            offset: 0,
            content: ReviewBlockContent::Comment {
                id: comment.id,
                text: Rope::from_str(&comment.text),
                side: CommentSide::Current,
                range,
            },
        });
    }
    blocks.sort_by_key(|block| (block.anchor, !block.at_start));
    let mut previous = None;
    let mut offset = 0;
    for block in &mut blocks {
        let group = (block.anchor, block.at_start);
        if previous != Some(group) {
            offset = 0;
        }
        block.offset = offset;
        offset += block.height();
        previous = Some(group);
    }
    blocks
}
