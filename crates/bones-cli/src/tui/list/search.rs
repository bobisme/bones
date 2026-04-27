impl ListView {
    fn refresh_semantic_search_ids(&mut self) -> Result<()> {
        // Bump generation to invalidate any in-flight background search.
        self.semantic_search_gen = self.semantic_search_gen.wrapping_add(1);
        self.semantic_refinement_rx = None;

        let query = self.filter.search_query.trim();
        self.last_searched_query = query.to_string();
        if query.is_empty() {
            self.semantic_search_ids.clear();
            self.semantic_search_active = false;
            self.search_refining = false;
            return Ok(());
        }

        let Some(conn) = query::try_open_projection(&self.db_path)? else {
            self.semantic_search_ids.clear();
            self.semantic_search_active = false;
            self.search_refining = false;
            return Ok(());
        };

        let effective_query =
            if !query.contains(' ') && !query.contains('*') && !query.contains(':') {
                format!("{query}*")
            } else {
                query.to_string()
            };

        // Tier 1: fast search (lexical + structural only) — runs synchronously.
        // Replace results atomically instead of clearing first to avoid flash.
        let fast_start = Instant::now();
        let fast_hits = match hybrid_search_fast(&effective_query, &conn, 200, 60) {
            Ok(hits) => hits,
            Err(err) => {
                tracing::warn!("bones fast slash search failed: {err:#}");
                self.semantic_search_ids.clear();
                self.semantic_search_active = false;
                self.search_refining = false;
                return Ok(());
            }
        };
        let fast_elapsed = fast_start.elapsed();
        self.semantic_search_ids = fast_hits.into_iter().map(|hit| hit.item_id).collect();
        self.semantic_search_active = true;
        tracing::debug!(
            query = %effective_query,
            count = self.semantic_search_ids.len(),
            elapsed_us = fast_elapsed.as_micros(),
            "tier-1 fast search complete"
        );

        if let Some(model) = self.semantic_model.clone() {
            // Tier 2: spawn background thread for full semantic refinement.
            self.search_refining = true;
            let db_path = self.db_path.clone();
            let query_owned = effective_query;
            let (tx, rx) = std::sync::mpsc::channel();
            self.semantic_refinement_rx = Some(rx);

            std::thread::spawn(move || {
                let refine_start = Instant::now();
                let conn = match query::try_open_projection(&db_path) {
                    Ok(Some(c)) => c,
                    _ => return,
                };
                let hits = match hybrid_search(&query_owned, &conn, Some(&model), 200, 60) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::debug!("tier-2 semantic refinement failed: {e:#}");
                        return;
                    }
                };
                let ids: Vec<String> = hits.into_iter().map(|h| h.item_id).collect();
                tracing::debug!(
                    count = ids.len(),
                    elapsed_ms = refine_start.elapsed().as_millis() as u64,
                    "tier-2 semantic refinement complete"
                );
                let _ = tx.send(ids);
            });
        } else {
            self.search_refining = false;
        }

        Ok(())
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search_buf = self.search_prev_query.clone();
                self.search_cursor = char_len(&self.search_prev_query);
                self.filter.search_query = self.search_prev_query.clone();
                let _ = self.refresh_semantic_search_ids();
                self.apply_filter_and_sort();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                self.filter.search_query = self.search_buf.clone();
                let _ = self.refresh_semantic_search_ids();
                self.apply_filter_and_sort();
                self.input_mode = InputMode::Normal;
            }
            _ => {
                let changed =
                    edit_single_line_readline(&mut self.search_buf, &mut self.search_cursor, key);
                if changed {
                    self.filter.search_query = self.search_buf.clone();
                    let _ = self.refresh_semantic_search_ids();
                    self.apply_filter_and_sort();
                }
            }
        }
    }

}
