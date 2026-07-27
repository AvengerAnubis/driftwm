use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};

use super::DriftWm;

/// A layer surface pinned to a canvas position instead of being anchored
/// via LayerMap. Created when a layer's namespace matches a rule with `position`.
pub struct CanvasLayer {
    pub surface: smithay::desktop::LayerSurface,
    /// Rule position (Y-up, window-centered) — converted to canvas coords after first commit.
    pub rule_position: (i32, i32),
    /// Internal canvas position (Y-down, top-left). None until first commit reveals size.
    pub position: Option<Point<i32, Logical>>,
    pub namespace: String,
}

impl DriftWm {
    /// Layer-shell surfaces of `layer` on `output` with their mapped
    /// geometries, topmost first. The protocol has no z-order within a
    /// wlr-layer, so map order decides (newest on top) unless `layer_order`
    /// window rules rank surfaces explicitly (higher on top; ties keep map
    /// order). Render, hit-testing, and focus scans all use this, so visual
    /// z, input z, and focus priority can't disagree.
    pub fn layers_on_sorted(
        &self,
        output: &Output,
        layer: smithay::wayland::shell::wlr_layer::Layer,
    ) -> Vec<(
        smithay::desktop::LayerSurface,
        smithay::utils::Rectangle<i32, smithay::utils::Logical>,
    )> {
        let map = smithay::desktop::layer_map_for_output(output);
        // Rule resolution globs per surface; skip it when no rule ranks layers.
        let has_orders = self
            .config
            .window_rules
            .iter()
            .any(|r| r.layer_order.is_some());
        let mut surfaces: Vec<_> = map
            .layers_on(layer)
            .enumerate()
            .map(|(map_idx, s)| {
                let order = if has_orders {
                    self.config
                        .resolve_window_rules(s.namespace(), "")
                        .and_then(|r| r.layer_order)
                        .unwrap_or(0)
                } else {
                    0
                };
                (
                    order,
                    map_idx,
                    s.clone(),
                    map.layer_geometry(s).unwrap_or_default(),
                )
            })
            .collect();
        surfaces.sort_by_key(|&(order, map_idx, ..)| (order, map_idx));
        surfaces.reverse();
        surfaces
            .into_iter()
            .map(|(_, _, s, geo)| (s, geo))
            .collect()
    }

    /// Indices into `canvas_layers`, topmost first: higher `layer_order`
    /// rules stack above; ties keep the existing first-mapped-on-top order.
    /// Shared by canvas-layer rendering and hit-testing, like
    /// [`Self::layers_on_sorted`] for the screen-space layers. Rules resolve
    /// per instance (same prefix-count as the render path), so two positioned
    /// rules for the same namespace can order their instances independently.
    pub fn canvas_layer_indices_sorted(&self) -> Vec<usize> {
        if self.canvas_layers.len() < 2 {
            return (0..self.canvas_layers.len()).collect();
        }
        let has_orders = self
            .config
            .window_rules
            .iter()
            .any(|r| r.layer_order.is_some());
        let mut indices: Vec<(i32, usize)> = self
            .canvas_layers
            .iter()
            .enumerate()
            .map(|(idx, cl)| {
                let order = if has_orders {
                    let instance_idx = self.canvas_layers[..idx]
                        .iter()
                        .filter(|other| other.namespace == cl.namespace)
                        .count();
                    self.config
                        .resolve_window_rules_for_layer_instance(&cl.namespace, "", instance_idx)
                        .and_then(|r| r.layer_order)
                        .unwrap_or(0)
                } else {
                    0
                };
                (order, idx)
            })
            .collect();
        indices.sort_by_key(|&(order, idx)| (std::cmp::Reverse(order), idx));
        indices.into_iter().map(|(_, idx)| idx).collect()
    }

    pub(crate) fn layer_interactivity(
        &self,
        surface: &WlSurface,
    ) -> Option<smithay::wayland::shell::wlr_layer::KeyboardInteractivity> {
        for cl in &self.canvas_layers {
            if cl.surface.wl_surface() == surface {
                return Some(cl.surface.cached_state().keyboard_interactivity);
            }
        }
        for output in self.space.outputs() {
            let map = smithay::desktop::layer_map_for_output(output);
            for l in map.layers() {
                if l.wl_surface() == surface {
                    return Some(l.cached_state().keyboard_interactivity);
                }
            }
        }
        None
    }
}
