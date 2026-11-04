//! Pixel → terminal-cell arithmetic. The one piece of the shell that needs no
//! GPU, no server and no window, so it is where the tests live.

/// What one character cell costs in pixels at the current font size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub cell_w: f64,
    pub cell_h: f64,
}

impl Metrics {
    /// Guards against a zero or absurd advance from a missing font, which would
    /// otherwise divide by zero into an insane column count.
    pub fn sane(self) -> Self {
        Self {
            cell_w: self.cell_w.max(4.0),
            cell_h: self.cell_h.max(8.0),
        }
    }
}

/// How many cells fit in `width` × `height` pixels, inside `pad` on all sides.
/// Clamped to the range a terminal can actually be.
pub fn fit_grid(width: f64, height: f64, pad: f64, m: Metrics) -> (u16, u16) {
    let m = m.sane();
    let inner_w = (width - pad * 2.0).max(m.cell_w);
    let inner_h = (height - pad * 2.0).max(m.cell_h);
    let cols = (inner_w / m.cell_w).floor() as i64;
    let rows = (inner_h / m.cell_h).floor() as i64;
    (cols.clamp(10, 500) as u16, rows.clamp(1, 300) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics {
        cell_w: 8.0,
        cell_h: 17.0,
    };

    #[test]
    fn divides_inside_the_padding() {
        assert_eq!(fit_grid(800.0, 340.0, 4.0, M), (99, 19));
    }

    #[test]
    fn never_reports_fewer_than_ten_columns() {
        assert_eq!(fit_grid(10.0, 10.0, 4.0, M).0, 10);
    }

    #[test]
    fn survives_a_zero_width_font() {
        let (cols, rows) = fit_grid(
            800.0,
            340.0,
            4.0,
            Metrics {
                cell_w: 0.0,
                cell_h: 0.0,
            },
        );
        assert_eq!((cols, rows), (198, 41));
    }
}
