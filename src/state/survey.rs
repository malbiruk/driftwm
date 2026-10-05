//! Stable window hints for a single zoom-to-fit overview.
use super::{ClusterMember, DriftWm, NavZoom, output_state};
use driftwm::window_ext::WindowExt;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use std::collections::HashMap;

pub(crate) struct SurveyHint {
    pub target: ClusterMember,
    pub label: String,
    pub buffer: Option<(i32, bool, MemoryRenderBuffer)>,
}

impl DriftWm {
    pub(crate) fn survey_active(&self) -> bool {
        self.config.zoom_survey_labels
            && !self.survey_hints.is_empty()
            && self.survey_output.as_ref().is_some_and(|o| {
                self.active_output().as_ref() == Some(o)
                    && output_state(o).overview_return.is_some()
            })
    }

    pub(crate) fn begin_survey(&mut self) {
        self.clear_survey();
        if !self.config.zoom_survey_labels || self.overview_return().is_none() {
            return;
        }
        self.survey_output = self.active_output();
        let windows: Vec<_> = self
            .stage
            .windows()
            .filter(|w| self.is_canvas_window(*w))
            .filter(|w| self.snap_rect_for(w).is_some())
            .cloned()
            .collect();
        let names: Vec<_> = windows
            .iter()
            .map(|w| w.app_id_or_class().unwrap_or_default())
            .collect();
        let labels = assign_labels(&names);
        self.survey_hints = windows
            .iter()
            .zip(labels)
            .map(|(w, label)| SurveyHint {
                target: ClusterMember::from_element(w),
                label,
                buffer: None,
            })
            .collect();
    }

    pub(crate) fn clear_survey(&mut self) {
        self.survey_output = None;
        self.survey_hints.clear();
        self.survey_prefix.clear();
        self.survey_pending = None;
    }

    pub(crate) fn survey_key(&mut self, raw: u32) -> bool {
        if !self.survey_active() {
            return false;
        }
        if raw == smithay::input::keyboard::keysyms::KEY_BackSpace {
            self.survey_prefix.pop();
            return true;
        }
        let Some(c) = char::from_u32(raw).filter(char::is_ascii_alphanumeric) else {
            return false;
        };
        self.survey_prefix.push(c.to_ascii_uppercase());
        if let Some(hint) = self
            .survey_hints
            .iter()
            .find(|h| h.label == self.survey_prefix)
        {
            self.survey_pending = Some(hint.target.clone());
        } else if !self
            .survey_hints
            .iter()
            .any(|h| h.label.starts_with(&self.survey_prefix))
        {
            self.survey_prefix.clear();
        }
        true
    }

    // Run after keyboard.input has released its mutex; navigation sets keyboard focus.
    pub(crate) fn perform_survey_pick(&mut self) {
        let Some(target) = self.survey_pending.take() else {
            return;
        };
        if let Some(window) = target.resolve(&self.stage).filter(|window| {
            self.is_canvas_window(window) && self.stage.position_of(window).is_some()
        }) {
            self.set_overview_return(None);
            self.clear_survey();
            self.navigate_to_element(&window, NavZoom::Reset);
        } else {
            self.survey_prefix.clear();
        }
    }
}

fn assign_labels(names: &[String]) -> Vec<String> {
    const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let alphabet: Vec<char> = ALPHABET.chars().collect();
    let mut length = 1;
    let mut capacity = alphabet.len();
    while capacity < names.len() {
        length += 1;
        capacity = capacity.saturating_mul(alphabet.len());
    }
    let mut next_suffix = HashMap::new();
    names
        .iter()
        .map(|name| {
            let app = name.rsplit(['.', '/']).next().unwrap_or(name);
            let preferred = app
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_uppercase());
            let first: Vec<_> = preferred.chain(alphabet.iter().copied()).collect();
            for c in first {
                let suffix_count = alphabet.len().pow(length - 1);
                let next = next_suffix.entry(c).or_insert(0);
                if *next >= suffix_count {
                    continue;
                }
                let mut n = *next;
                *next += 1;
                let mut suffix = vec!['A'; (length - 1) as usize];
                for slot in suffix.iter_mut().rev() {
                    *slot = alphabet[n % alphabet.len()];
                    n /= alphabet.len();
                }
                return std::iter::once(c).chain(suffix).collect();
            }
            unreachable!("label capacity covers all windows")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    #[test]
    fn labels_prefer_app_letters_and_are_unique_prefix_free() {
        assert_eq!(
            assign_labels(&[
                "org.mozilla.Firefox".into(),
                "Spotify".into(),
                "Firefox".into()
            ]),
            vec!["F", "S", "I"]
        );
        let names = vec!["browser".to_string(); 1400];
        let labels = assign_labels(&names);
        assert_eq!(labels.iter().collect::<HashSet<_>>().len(), names.len());
        assert!(labels.iter().all(|s| s.len() == 3));
    }
}
