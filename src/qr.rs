//! QR codes painted with squares (setting up two-step verification from a
//! phone, and invitations). Always black modules on a white background, with
//! a light margin around them, so that cameras read them with the dark theme
//! too.

use gpui::{
    AnyElement, Bounds, IntoElement, ParentElement, Pixels, Styled, canvas, div, fill, point, px,
    size, white,
};

/// Light margin modules around the code (the standard asks for 4).
pub const QUIET: usize = 4;

/// Horizontal run of dark modules, already offset by the margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub row: usize,
    pub col: usize,
    pub len: usize,
}

/// Layout of a QR code: total side (with margin) and dark runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Modules per side, margin included.
    pub side: usize,
    pub runs: Vec<Run>,
}

/// Computes the layout: joins the contiguous dark modules of each row into
/// runs (far fewer rectangles to paint) and leaves `quiet` margin modules on
/// each side.
pub fn layout(matrix: &[Vec<bool>], quiet: usize) -> Layout {
    let n = matrix.len();
    let mut runs = Vec::new();
    for (y, row) in matrix.iter().enumerate() {
        let mut x = 0;
        while x < row.len() {
            if !row[x] {
                x += 1;
                continue;
            }
            let start = x;
            while x < row.len() && row[x] {
                x += 1;
            }
            runs.push(Run {
                row: y + quiet,
                col: start + quiet,
                len: x - start,
            });
        }
    }
    Layout {
        side: n + quiet * 2,
        runs,
    }
}

/// Module size (in whole pixels, so that edges are sharp) for the code to
/// fit in `max` pixels.
pub fn module_size(side: usize, max: f32) -> f32 {
    if side == 0 {
        return 1.0;
    }
    (max / side as f32).floor().max(1.0)
}

/// Element with the QR code of `text`, at most `max` pixels per side. If the
/// text does not fit in a QR code, a notice.
pub fn element(text: &str, max: f32) -> AnyElement {
    let Some(matrix) = termoak_client::qr::matrix(text) else {
        return div().text_sm().child(t!("qr.error")).into_any_element();
    };
    let layout = layout(&matrix, QUIET);
    let module = module_size(layout.side, max);
    let total = module * layout.side as f32;
    div()
        .flex_shrink_0()
        .size(px(total))
        .child(
            canvas(
                |_, _, _| {},
                move |bounds: Bounds<Pixels>, _, window, _| {
                    let origin = point(bounds.origin.x.floor(), bounds.origin.y.floor());
                    window.paint_quad(fill(
                        Bounds::new(origin, size(px(total), px(total))),
                        white(),
                    ));
                    let black = gpui::black();
                    for run in &layout.runs {
                        let b = Bounds::new(
                            point(
                                origin.x + px(run.col as f32 * module),
                                origin.y + px(run.row as f32 * module),
                            ),
                            size(px(run.len as f32 * module), px(module)),
                        );
                        window.paint_quad(fill(b, black));
                    }
                },
            )
            .size_full(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_cover_every_dark_module_once() {
        let m = termoak_client::qr::matrix("otpauth://totp/Termoak:ana?secret=GEZDGNBV").unwrap();
        let l = layout(&m, QUIET);
        assert_eq!(l.side, m.len() + 2 * QUIET);
        let mut painted = vec![vec![false; l.side]; l.side];
        for r in &l.runs {
            assert!(r.len > 0);
            for x in r.col..r.col + r.len {
                assert!(!painted[r.row][x], "module painted twice");
                painted[r.row][x] = true;
            }
        }
        for (y, row) in painted.iter().enumerate() {
            for (x, dark) in row.iter().enumerate() {
                let inside =
                    (QUIET..QUIET + m.len()).contains(&y) && (QUIET..QUIET + m.len()).contains(&x);
                let expected = inside && m[y - QUIET][x - QUIET];
                assert_eq!(*dark, expected, "module ({x}, {y})");
            }
        }
    }

    #[test]
    fn contiguous_modules_merge_into_one_run() {
        let m = vec![
            vec![true, true, false, true],
            vec![false, false, false, false],
            vec![true, true, true, true],
            vec![false, true, false, false],
        ];
        let l = layout(&m, 1);
        assert_eq!(l.side, 6);
        assert_eq!(
            l.runs,
            vec![
                Run {
                    row: 1,
                    col: 1,
                    len: 2
                },
                Run {
                    row: 1,
                    col: 4,
                    len: 1
                },
                Run {
                    row: 3,
                    col: 1,
                    len: 4
                },
                Run {
                    row: 4,
                    col: 2,
                    len: 1
                },
            ]
        );
    }

    #[test]
    fn module_size_is_whole_and_fits() {
        assert_eq!(module_size(41, 220.), 5.);
        assert_eq!(module_size(41, 20.), 1.);
        assert!(module_size(37, 240.) * 37. <= 240.);
        assert_eq!(module_size(0, 100.), 1.);
    }
}
