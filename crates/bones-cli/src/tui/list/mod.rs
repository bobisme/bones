//! TUI list view for bones.
//!
//! Provides a full-screen terminal UI with:
//! - Filterable nested bones list with slash search
//! - Right-side detail pane
//! - Key bindings: j/k navigate or scroll, / search, F filter, a add bone, D show/hide done, q quit

#![allow(
    clippy::similar_names,
    clippy::match_same_arms,
    clippy::manual_let_else,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::assigning_clones,
    clippy::items_after_statements,
    clippy::option_if_let_else,
    clippy::needless_pass_by_ref_mut,
    clippy::struct_excessive_bools,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::map_unwrap_or
)]

use crate::{agent, validate};
use anyhow::{Context, Result};
use bones_core::config::load_project_config;
use bones_core::db::query::{self, ItemFilter, QueryItem, SortOrder};
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_search::fusion::{hybrid_search, hybrid_search_fast};
use bones_search::semantic::SemanticModel;
use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    collections::{HashMap, HashSet},
    io::Write as _,
    path::Path,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde_json::json;

use super::actions;

include!("state.rs");
include!("data.rs");
include!("search.rs");
include!("input.rs");
include!("render.rs");
include!("tests.rs");
