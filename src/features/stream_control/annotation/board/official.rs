//! Official Draw has no background primitive: translate one board intent into pen paths.
use super::*;
#[path = "branding.rs"]
mod branding;
const BRUSH_WIDTH: f32 = 64.;
#[derive(Clone)]
pub(super) struct Geometry {
    stroke: Stroke,
    logical_height: f32,
    marks: Vec<Stroke>,
}
fn fill(id: u32, screen: i32, metrics: Metrics, rgb: [u8; 3]) -> Result<Geometry> {
    let height = metrics.logical_height();
    let rows = (height / (BRUSH_WIDTH * 0.5)).ceil().max(1.) as usize;
    if rows > MAX_POINTS / 6 - 1 {
        bail!("当前屏幕尺寸超出白板绘制范围");
    }
    let mut points = Vec::with_capacity((rows + 1) * 6);
    for row in 0..=rows {
        let y = row as f32 / rows as f32;
        let (left, right) = if row % 2 == 0 { (0., 1.) } else { (1., 0.) };
        // Duplicate turns preserve coverage at the edges under host Bezier smoothing.
        points.extend([Point { x: left, y }; 3]);
        points.extend([Point { x: right, y }; 3]);
    }
    Ok(Geometry {
        stroke: Stroke {
            id,
            screen,
            points,
            style: Style {
                argb: u32::from_be_bytes([255, rgb[0], rgb[1], rgb[2]]),
                width: BRUSH_WIDTH,
            },
        },
        logical_height: height,
        marks: Vec::new(),
    })
}

pub(super) fn plan(
    boards: &HashMap<i32, Board>,
    screen: i32,
    metrics: Metrics,
    color: Option<u32>,
) -> Result<Option<(Option<Board>, Vec<PbDrawRequest>)>> {
    let old = boards.get(&screen);
    if old.map(|b| b.color) == color && old.is_none_or(|b| b.metrics == metrics) {
        return Ok(None);
    }
    let previous = old.and_then(|b| b.official.as_ref());
    let mut requests = Vec::new();
    let mut value = if let Some(argb) = color {
        let id = if let Some(geometry) = previous {
            geometry.stroke.id
        } else {
            (1..=BOARD_IDS)
                .step_by(BOARD_SLOT_IDS as usize)
                .find(|id| {
                    !boards
                        .values()
                        .filter_map(|b| b.official.as_ref())
                        .any(|b| b.stroke.id == *id)
                })
                .ok_or_else(|| anyhow!("白板数量已达到上限"))?
        };
        let [_, r, g, b] = argb.to_be_bytes();
        let geometry =
            if let Some(old) = previous.filter(|b| metrics.logical_height() <= b.logical_height) {
                let mut geometry = old.clone();
                geometry.stroke.style.argb = argb;
                if old.stroke.style.argb != argb {
                    requests.push(stroke_request(
                        &geometry.stroke,
                        &[*geometry.stroke.points.last().unwrap()],
                    ));
                }
                geometry
            } else {
                let geometry = fill(id, screen, metrics, [r, g, b])?;
                if previous.is_some() {
                    requests.push(clear_request(2, id, Some(screen)));
                }
                requests.extend(
                    geometry
                        .stroke
                        .points
                        .chunks(4096)
                        .map(|points| stroke_request(&geometry.stroke, points)),
                );
                geometry
            };
        Some(Board {
            color: argb,
            metrics,
            official: Some(geometry),
        })
    } else {
        let Some(old) = previous else {
            return Ok(None);
        };
        requests.push(clear_request(2, old.stroke.id, Some(screen)));
        None
    };
    // Remove old lettering before backdrop edits, then create the new header.
    requests.splice(
        0..0,
        previous
            .into_iter()
            .flat_map(|b| b.marks.iter().map(|m| clear_request(2, m.id, Some(screen)))),
    );
    if let Some(board) = &mut value {
        let geometry = board.official.as_mut().unwrap();
        let [_, r, g, b] = board.color.to_be_bytes();
        geometry.marks = branding::build(geometry.stroke.id, screen, metrics, [r, g, b])?;
        requests.extend(geometry.marks.iter().flat_map(|m| {
            m.points
                .chunks(4096)
                .map(|points| stroke_request(m, points))
        }));
    }
    Ok(Some((value, requests)))
}
