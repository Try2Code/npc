//! Shooting data, formatted the way a photographer reads it rather than the
//! way it is stored -- shared between the TUI and any other UI that wants to
//! show a photo's EXIF the same way (a native GUI, say).

use crate::db::Catalog;

/// Shooting data, as it is displayed rather than as it is stored.
#[derive(Debug, Clone, Default)]
pub struct Exif {
    pub captured: Option<i64>,
    pub exposure: Option<f64>,
    pub fnumber: Option<f64>,
    pub iso: Option<i64>,
    pub bias: Option<f64>,
    pub focal: Option<f64>,
    pub lens: String,
    pub camera: String,
    pub width: i64,
    pub height: i64,
    pub orientation: u16,
}

impl Exif {
    /// True when the frame is taller than it is wide *as displayed*, which is
    /// what decides whether an info panel goes beside it or beneath it.
    pub fn is_portrait(&self) -> bool {
        let (w, h) = if matches!(self.orientation, 5..=8) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        h > w && w > 0
    }

    /// The exposure quintet, in the order a photographer reads it.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = self.captured {
            parts.push(format_datetime(t));
        }
        if let Some(e) = self.exposure.filter(|e| *e > 0.0) {
            parts.push(format!("S {}", format_shutter(e)));
        }
        if let Some(f) = self.fnumber.filter(|f| *f > 0.0) {
            parts.push(format!("A f/{f:.1}"));
        }
        if let Some(i) = self.iso.filter(|i| *i > 0) {
            parts.push(format!("I {i}"));
        }
        if let Some(b) = self.bias {
            parts.push(format!("EV {}", format_ev(b)));
        }
        if let Some(f) = self.focal.filter(|f| *f > 0.0) {
            parts.push(format!("{f:.0}mm"));
        }
        parts.join("   ")
    }
}

/// Shooting data for one stack, read live from the catalog: whichever member
/// (RAW preferred over JPG) carries it.
pub fn for_stack(cat: &Catalog, stack_id: i64) -> Option<Exif> {
    cat.conn
        .query_row(
            "SELECT capture_time, exposure, fnumber, iso, exposure_bias, focal,
                    lens, camera, width, height,
                    COALESCE(?2, exif_orientation, 1)
             FROM file
             WHERE stack_id = ?1 AND present = 1
             ORDER BY CASE role WHEN 'raw' THEN 0 WHEN 'oocjpg' THEN 1 ELSE 2 END
             LIMIT 1",
            rusqlite::params![stack_id, None::<i64>],
            |r| {
                Ok(Exif {
                    captured: r.get(0)?,
                    exposure: r.get(1)?,
                    fnumber: r.get(2)?,
                    iso: r.get(3)?,
                    bias: r.get(4)?,
                    focal: r.get(5)?,
                    lens: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    camera: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    width: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    height: r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                    // EXIF orientation is one of 1-8; the column is i64 only
                    // because SQLite has no smaller integer type.
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "EXIF orientation is always 1-8"
                    )]
                    orientation: r.get::<_, Option<i64>>(10)?.unwrap_or(1) as u16,
                })
            },
        )
        .ok()
}

/// `1/400` below a second, `2.5"` above it -- how a camera displays it.
pub fn format_shutter(seconds: f64) -> String {
    if seconds >= 1.0 {
        format!("{seconds:.1}\"")
    } else {
        format!("1/{:.0}", (1.0 / seconds).round())
    }
}

/// Signed, because the sign is the whole point of exposure compensation.
pub fn format_ev(ev: f64) -> String {
    if ev.abs() < 0.01 {
        "0".to_string()
    } else {
        format!("{ev:+.1}")
    }
}

pub fn format_datetime(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn format_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // `d` is a day of month (1-31) and `m` a month (1-12) by construction of
    // Howard Hinnant's civil_from_days, whatever the input day number. Neither
    // can be negative or exceed u32, so these conversions cannot fail.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "day-of-month and month are 1-31 and 1-12 by construction"
    )]
    let (d, m) = (
        (doy - (153 * mp + 2) / 5 + 1) as u32,
        if mp < 10 { mp + 3 } else { mp - 9 } as u32,
    );
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_render_with_the_same_convention_as_capture_times() {
        assert_eq!(format_date(1_786_233_666), "2026-08-09");
        assert_eq!(format_date(0), "1970-01-01");
    }

    #[test]
    fn shutter_speeds_read_the_way_a_camera_shows_them() {
        assert_eq!(format_shutter(1.0 / 400.0), "1/400");
        assert_eq!(format_shutter(0.0025), "1/400");
        assert_eq!(format_shutter(1.0 / 13.0), "1/13");
        assert_eq!(format_shutter(1.0), "1.0\"");
        assert_eq!(format_shutter(2.5), "2.5\"");
    }

    /// Exposure compensation is meaningless without its sign, and Nikon works
    /// in thirds -- -2/3 EV arrives as -0.6666 and must read as the camera
    /// displays it.
    #[test]
    fn exposure_compensation_keeps_its_sign() {
        assert_eq!(format_ev(0.0), "0");
        assert_eq!(format_ev(-0.666_666_7), "-0.7");
        assert_eq!(format_ev(-0.333_333_3), "-0.3");
        assert_eq!(format_ev(1.0), "+1.0");
        assert_eq!(format_ev(-1.666_666_6), "-1.7");
    }

    #[test]
    fn the_exposure_line_is_labelled() {
        let e = Exif {
            captured: Some(1_786_233_666),
            exposure: Some(0.0025),
            fnumber: Some(4.5),
            iso: Some(2500),
            bias: Some(-0.666_666_7),
            focal: Some(200.0),
            ..Default::default()
        };
        let line = e.line();
        assert!(line.contains("S 1/400"), "{line}");
        assert!(line.contains("A f/4.5"), "{line}");
        assert!(line.contains("I 2500"), "{line}");
        assert!(line.contains("EV -0.7"), "{line}");
        assert!(line.contains("200mm"), "{line}");
        assert!(line.contains("2026-08-09"), "{line}");
    }

    /// Missing fields are omitted rather than shown as zero. One file in the
    /// library has no ISO recorded, and `I 0` would be a lie.
    #[test]
    fn missing_fields_are_omitted_not_zeroed() {
        let e = Exif {
            captured: Some(1_786_233_666),
            iso: Some(0),
            fnumber: Some(0.0),
            exposure: None,
            ..Default::default()
        };
        let line = e.line();
        assert!(!line.contains("I 0"), "{line}");
        assert!(!line.contains("f/0"), "{line}");
        assert!(!line.contains("S "), "{line}");
    }

    /// The layout decision: orientation, not stored dimensions, decides
    /// whether a frame displays portrait. A D750 writes 6016x4016 with
    /// orientation 8 for a portrait shot.
    #[test]
    fn portrait_is_decided_after_rotation() {
        let landscape = Exif {
            width: 6016,
            height: 4016,
            orientation: 1,
            ..Default::default()
        };
        assert!(!landscape.is_portrait());

        let rotated = Exif {
            width: 6016,
            height: 4016,
            orientation: 8,
            ..Default::default()
        };
        assert!(
            rotated.is_portrait(),
            "orientation 8 must display as portrait"
        );

        let unknown = Exif::default();
        assert!(
            !unknown.is_portrait(),
            "no dimensions must not claim portrait"
        );
    }

    /// `0.0` EV is real (shot at metered exposure) and must not be
    /// indistinguishable from "no value recorded at all".
    #[test]
    fn zero_ev_is_shown_not_omitted() {
        let e = Exif {
            bias: Some(0.0),
            ..Default::default()
        };
        assert!(e.line().contains("EV 0"), "{}", e.line());
    }

    /// The epoch boundary and a leap-year date, since `civil` is hand-rolled
    /// date math with no library to lean on.
    #[test]
    fn civil_conversion_handles_known_edges() {
        assert_eq!(format_date(0), "1970-01-01");
        // 2024-02-29, a leap day.
        assert_eq!(format_date(1_709_164_800), "2024-02-29");
    }
}
