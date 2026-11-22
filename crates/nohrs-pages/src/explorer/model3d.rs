//! 3D models in the preview pane: OBJ, STL, glTF and FBX read by their own
//! crates and drawn by a small rasterizer on the CPU, turned by dragging.
//!
//! No GPU path: gpui has no way to put a 3D surface in an element, and the
//! pane only needs a look at the shape, not a viewer. A flat-shaded image of
//! the size of the pane, redrawn on a worker thread split into bands, keeps
//! up with a drag on models of a few hundred thousand triangles.

use gpui::{Context, RenderImage, Task};
use nohrs_core::t;
use std::sync::Arc;

use super::ExplorerPane;

/// Longest side of the image drawn, in pixels: past this the drag stops
/// keeping up, and the pane is seldom wider.
const MAX_SIDE: f32 = 1400.0;

/// Whether the preview draws `path` as a 3D model.
pub(crate) fn is_model(path: &str) -> bool {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(extension.as_str(), "obj" | "stl" | "gltf" | "glb" | "fbx")
}

/// A model's triangles, centred on the origin and scaled into the unit
/// sphere, each with its material's colour.
pub(crate) struct Mesh {
    positions: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
    colors: Vec<[u8; 3]>,
}

/// The model in the preview and how it is turned.
pub(crate) struct ModelPreview {
    pub mesh: Option<Arc<Mesh>>,
    pub error: Option<String>,
    /// Turn about the vertical axis and tilt, in radians.
    pub yaw: f32,
    pub pitch: f32,
    pub image: Option<Arc<RenderImage>>,
    /// Size of the view in device pixels, set at paint.
    pub view: std::rc::Rc<std::cell::Cell<(u32, u32)>>,
    /// The view and zoom the image on screen was drawn for.
    drawn: Option<(f32, f32, f32, (u32, u32))>,
    /// A drawing is on its way; another asked for meanwhile waits for it.
    drawing: bool,
    _load: Task<()>,
    _draw: Option<Task<()>>,
}

impl ModelPreview {
    /// Triangles in the model, for the line under it.
    pub(crate) fn triangle_count(&self) -> usize {
        self.mesh.as_ref().map_or(0, |mesh| mesh.triangles.len())
    }
}

impl ExplorerPane {
    /// Reads the model at `path` off the UI thread and draws it once read.
    pub(crate) fn open_model_preview(&mut self, path: String, cx: &mut Context<Self>) {
        // A thread of its own, as the player's: on the shared background
        // pool the read waited behind the window's startup work (thumbnails,
        // listings, the index) and a model opened at launch sat on "Abrindo…"
        // for seconds although reading it takes a fraction of one.
        let (sender, receiver) = async_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("nohrs-model".into())
            .spawn(move || {
                sender.send_blocking(load(&path)).ok();
            });
        if let Err(error) = spawned {
            tracing::error!("could not start reading the model: {error}");
        }
        let load = cx.spawn(async move |this, cx| {
            let Ok(result) = receiver.recv().await else {
                return;
            };
            this.update(cx, |pane, cx| {
                let Some(model) = pane.preview_model.as_mut() else {
                    return;
                };
                match result {
                    Ok(mesh) => model.mesh = Some(Arc::new(mesh)),
                    Err(error) => model.error = Some(error),
                }
                pane.draw_model(cx);
                cx.notify();
            })
            .ok();
        });
        self.preview_model = Some(ModelPreview {
            mesh: None,
            error: None,
            // A three-quarter view: straight on, a box shows only one face.
            yaw: -0.6,
            pitch: 0.35,
            image: None,
            view: Default::default(),
            drawn: None,
            drawing: false,
            _load: load,
            _draw: None,
        });
    }

    /// Lets go of the model and its image when the preview moves on.
    pub(crate) fn close_model_preview(&mut self, cx: &mut gpui::App) {
        if let Some(model) = self.preview_model.take()
            && let Some(image) = model.image
        {
            cx.drop_image(image, None);
        }
    }

    /// Turns the model by a drag of `dx`, `dy` logical pixels.
    pub(crate) fn turn_model(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        if let Some(model) = self.preview_model.as_mut() {
            model.yaw += dx * 0.01;
            model.pitch = (model.pitch + dy * 0.01).clamp(-1.55, 1.55);
            self.draw_model(cx);
        }
    }

    /// Back to the view the model opened with.
    pub(crate) fn reset_model_view(&mut self, cx: &mut Context<Self>) {
        self.preview_zoom = 1.0;
        if let Some(model) = self.preview_model.as_mut() {
            (model.yaw, model.pitch) = (-0.6, 0.35);
            self.draw_model(cx);
        }
    }

    /// Draws the model for its current view, zoom and size, unless the image
    /// on screen already is that. One drawing at a time: the one asked for
    /// during it starts when it ends, with whatever the view is by then, so
    /// a fast drag skips the views it went past instead of queueing them.
    pub(crate) fn draw_model(&mut self, cx: &mut Context<Self>) {
        let zoom = self.preview_zoom;
        let Some(model) = self.preview_model.as_mut() else {
            return;
        };
        let Some(mesh) = model.mesh.clone() else {
            return;
        };
        let (width, height) = model.view.get();
        if width == 0 || height == 0 || model.drawing {
            return;
        }
        let wanted = (model.yaw, model.pitch, zoom, (width, height));
        if model.drawn == Some(wanted) {
            return;
        }
        // Drawn smaller than the view when the pane is large, and shown
        // stretched: a drag matters more than the last bit of sharpness.
        let scale = (MAX_SIDE / width.max(height) as f32).min(1.0);
        let (draw_w, draw_h) = (
            ((width as f32) * scale) as u32,
            ((height as f32) * scale) as u32,
        );
        let (yaw, pitch) = (model.yaw, model.pitch);
        model.drawing = true;
        // Off the shared pool for the reason the read is (see
        // `open_model_preview`); `rasterize` spreads over its own threads.
        let (sender, receiver) = async_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("nohrs-model-draw".into())
            .spawn(move || {
                sender
                    .send_blocking(rasterize(
                        &mesh,
                        yaw,
                        pitch,
                        zoom,
                        draw_w.max(1),
                        draw_h.max(1),
                    ))
                    .ok();
            });
        if let Err(error) = spawned {
            tracing::error!("could not start drawing the model: {error}");
            model.drawing = false;
            return;
        }
        model._draw = Some(cx.spawn(async move |this, cx| {
            let Ok(image) = receiver.recv().await else {
                return;
            };
            this.update(cx, |pane, cx| {
                let Some(model) = pane.preview_model.as_mut() else {
                    return;
                };
                model.drawing = false;
                model.drawn = Some(wanted);
                if let Some(image) = image
                    && let Some(old) = model.image.replace(Arc::new(image))
                {
                    // The atlas keeps a copy of every image drawn.
                    cx.drop_image(old, None);
                }
                pane.draw_model(cx);
                cx.notify();
            })
            .ok();
        }));
    }
}

fn load(path: &str) -> Result<Mesh, String> {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mut mesh = match extension.as_str() {
        "obj" => load_obj(path)?,
        "stl" => load_stl(path)?,
        "fbx" => load_fbx(path)?,
        _ => load_gltf(path)?,
    };
    if mesh.triangles.is_empty() {
        return Err(t!("O modelo não tem triângulos").to_string());
    }
    normalize(&mut mesh.positions);
    Ok(mesh)
}

/// Grey of a surface with no material, light enough to show the shading.
const PLAIN: [u8; 3] = [200, 200, 205];

fn to_byte(channel: f32) -> u8 {
    (channel.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn load_obj(path: &str) -> Result<Mesh, String> {
    let options = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ignore_points: true,
        ignore_lines: true,
    };
    let (models, materials) = tobj::load_obj(path, &options).map_err(|error| error.to_string())?;
    // A missing or broken .mtl only costs the colours.
    let materials = materials.unwrap_or_default();
    let mut mesh = Mesh {
        positions: Vec::new(),
        triangles: Vec::new(),
        colors: Vec::new(),
    };
    for model in models {
        let base = mesh.positions.len() as u32;
        mesh.positions
            .extend(model.mesh.positions.as_chunks::<3>().0.iter().copied());
        let color = model
            .mesh
            .material_id
            .and_then(|id| materials.get(id))
            .and_then(|material| material.diffuse)
            .map_or(PLAIN, |[r, g, b]| [to_byte(r), to_byte(g), to_byte(b)]);
        for triangle in model.mesh.indices.as_chunks::<3>().0 {
            mesh.triangles
                .push([base + triangle[0], base + triangle[1], base + triangle[2]]);
            mesh.colors.push(color);
        }
    }
    Ok(mesh)
}

#[allow(clippy::disallowed_methods)]
fn load_stl(path: &str) -> Result<Mesh, String> {
    let mut file =
        std::io::BufReader::new(std::fs::File::open(path).map_err(|error| error.to_string())?);
    let stl = stl_io::read_stl(&mut file).map_err(|error| error.to_string())?;
    let positions = stl
        .vertices
        .iter()
        .map(|vertex| [vertex[0], vertex[1], vertex[2]])
        .collect();
    let triangles: Vec<[u32; 3]> = stl
        .faces
        .iter()
        .map(|face| {
            [
                face.vertices[0] as u32,
                face.vertices[1] as u32,
                face.vertices[2] as u32,
            ]
        })
        .collect();
    let colors = vec![PLAIN; triangles.len()];
    Ok(Mesh {
        positions,
        triangles,
        colors,
    })
}

/// A glTF's default scene, every mesh placed by its node's transform. Only
/// the buffers are read: textures would cost seconds of decoding for an
/// image drawn in flat colours.
fn load_gltf(path: &str) -> Result<Mesh, String> {
    let gltf = gltf::Gltf::open(path).map_err(|error| error.to_string())?;
    let base = std::path::Path::new(path).parent();
    let buffers = gltf::import_buffers(&gltf.document, base, gltf.blob.clone())
        .map_err(|error| error.to_string())?;
    let mut mesh = Mesh {
        positions: Vec::new(),
        triangles: Vec::new(),
        colors: Vec::new(),
    };
    let scene = gltf
        .document
        .default_scene()
        .or_else(|| gltf.document.scenes().next());
    let Some(scene) = scene else {
        return Ok(mesh);
    };
    let identity = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let mut stack: Vec<(gltf::Node, [[f32; 4]; 4])> =
        scene.nodes().map(|node| (node, identity)).collect();
    while let Some((node, parent)) = stack.pop() {
        let transform = multiply(&parent, &node.transform().matrix());
        if let Some(node_mesh) = node.mesh() {
            for primitive in node_mesh.primitives() {
                if primitive.mode() != gltf::mesh::Mode::Triangles {
                    continue;
                }
                let reader =
                    primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data.0[..]));
                let Some(points) = reader.read_positions() else {
                    continue;
                };
                let start = mesh.positions.len() as u32;
                mesh.positions
                    .extend(points.map(|point| apply(&transform, point)));
                let count = mesh.positions.len() as u32 - start;
                let [r, g, b, _] = primitive
                    .material()
                    .pbr_metallic_roughness()
                    .base_color_factor();
                let color = [to_byte(r), to_byte(g), to_byte(b)];
                let indices: Vec<u32> = match reader.read_indices() {
                    Some(indices) => indices.into_u32().collect(),
                    None => (0..count).collect(),
                };
                for triangle in indices.as_chunks::<3>().0 {
                    if triangle.iter().all(|&index| index < count) {
                        mesh.triangles.push([
                            start + triangle[0],
                            start + triangle[1],
                            start + triangle[2],
                        ]);
                        mesh.colors.push(color);
                    }
                }
            }
        }
        stack.extend(node.children().map(|child| (child, transform)));
    }
    Ok(mesh)
}

/// A binary FBX (7.x): every `Geometry` mesh, placed by the `Model` it hangs
/// from and that model's parents (translation, rotation, scale; pivots are
/// left out), in the colour of the model's first material. ASCII FBX is not
/// read: the only crate for it is the tree reader this uses, binary only.
#[allow(clippy::disallowed_methods)]
fn load_fbx(path: &str) -> Result<Mesh, String> {
    use fbxcel::low::v7400::AttributeValue;
    use fbxcel::tree::any::AnyTree;
    use std::collections::HashMap;

    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let tree = match AnyTree::from_seekable_reader(std::io::BufReader::new(file)) {
        Ok(AnyTree::V7400(_, tree, _)) => tree,
        Ok(_) => return Err(t!("Versão de FBX não suportada").to_string()),
        Err(_) => {
            return Err(
                t!("Só FBX binário é suportado (este parece ser FBX em texto)").to_string(),
            );
        }
    };
    let root = tree.root();
    let number = |value: &AttributeValue| match value {
        AttributeValue::F64(v) => Some(*v),
        AttributeValue::F32(v) => Some(f64::from(*v)),
        AttributeValue::I32(v) => Some(f64::from(*v)),
        AttributeValue::I64(v) => Some(*v as f64),
        _ => None,
    };
    let id_of = |node: &fbxcel::tree::v7400::NodeHandle<'_>| {
        node.attributes().first().and_then(|value| value.get_i64())
    };
    // `P` lines of a `Properties70`: name, type, label, flags, then values.
    let property = |node: &fbxcel::tree::v7400::NodeHandle<'_>, name: &str| -> Option<[f64; 3]> {
        let properties = node.first_child_by_name("Properties70")?;
        let line = properties
            .children_by_name("P")
            .find(|p| p.attributes().first().and_then(|v| v.get_string()) == Some(name))?;
        let values: Vec<f64> = line
            .attributes()
            .iter()
            .skip(4)
            .filter_map(number)
            .collect();
        (values.len() >= 3).then(|| [values[0], values[1], values[2]])
    };

    // Each geometry's own points and triangles, by object id.
    type Geometry = (Vec<[f32; 3]>, Vec<[u32; 3]>);
    let mut geometries: HashMap<i64, Geometry> = HashMap::new();
    let mut models: HashMap<i64, [[f32; 4]; 4]> = HashMap::new();
    let mut materials: HashMap<i64, [u8; 3]> = HashMap::new();
    if let Some(objects) = root.first_child_by_name("Objects") {
        for object in objects.children() {
            let Some(id) = id_of(&object) else { continue };
            match object.name() {
                "Geometry" => {
                    let vertices = object.first_child_by_name("Vertices").and_then(|node| {
                        node.attributes()
                            .first()
                            .and_then(|v| v.get_arr_f64())
                            .map(<[f64]>::to_vec)
                    });
                    let indices =
                        object
                            .first_child_by_name("PolygonVertexIndex")
                            .and_then(|node| {
                                node.attributes()
                                    .first()
                                    .and_then(|v| v.get_arr_i32())
                                    .map(<[i32]>::to_vec)
                            });
                    let (Some(vertices), Some(indices)) = (vertices, indices) else {
                        continue;
                    };
                    let positions: Vec<[f32; 3]> = vertices
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .map(|p| [p[0] as f32, p[1] as f32, p[2] as f32])
                        .collect();
                    geometries.insert(id, (positions, fan_triangulate(&indices)));
                }
                "Model" => {
                    let translation = property(&object, "Lcl Translation").unwrap_or([0.0; 3]);
                    let rotation = property(&object, "Lcl Rotation").unwrap_or([0.0; 3]);
                    let pre_rotation = property(&object, "PreRotation").unwrap_or([0.0; 3]);
                    let scaling = property(&object, "Lcl Scaling").unwrap_or([1.0; 3]);
                    let local = multiply(
                        &translate(translation),
                        &multiply(
                            &multiply(&euler_xyz(pre_rotation), &euler_xyz(rotation)),
                            &scale(scaling),
                        ),
                    );
                    models.insert(id, local);
                }
                "Material" => {
                    if let Some([r, g, b]) =
                        property(&object, "DiffuseColor").or_else(|| property(&object, "Diffuse"))
                    {
                        materials.insert(
                            id,
                            [to_byte(r as f32), to_byte(g as f32), to_byte(b as f32)],
                        );
                    }
                }
                _ => {}
            }
        }
    }

    // `C "OO", child, parent`: geometry -> model, model -> parent model,
    // material -> model.
    let mut parent_of: HashMap<i64, i64> = HashMap::new();
    let mut geometry_models: Vec<(i64, i64)> = Vec::new();
    let mut model_color: HashMap<i64, [u8; 3]> = HashMap::new();
    if let Some(connections) = root.first_child_by_name("Connections") {
        for line in connections.children_by_name("C") {
            let attributes = line.attributes();
            if attributes.first().and_then(|v| v.get_string()) != Some("OO") {
                continue;
            }
            let (Some(child), Some(parent)) = (
                attributes.get(1).and_then(|v| v.get_i64()),
                attributes.get(2).and_then(|v| v.get_i64()),
            ) else {
                continue;
            };
            if geometries.contains_key(&child) && models.contains_key(&parent) {
                geometry_models.push((child, parent));
            } else if models.contains_key(&child) && models.contains_key(&parent) {
                parent_of.insert(child, parent);
            } else if let Some(&color) = materials.get(&child)
                && models.contains_key(&parent)
            {
                model_color.entry(parent).or_insert(color);
            }
        }
    }
    let world = |model: i64| {
        let mut matrix = models[&model];
        let mut at = model;
        // Bounded: a malformed file could connect models in a loop.
        for _ in 0..64 {
            let Some(&parent) = parent_of.get(&at) else {
                break;
            };
            matrix = multiply(&models[&parent], &matrix);
            at = parent;
        }
        matrix
    };

    let mut mesh = Mesh {
        positions: Vec::new(),
        triangles: Vec::new(),
        colors: Vec::new(),
    };
    // A geometry no model places is drawn where it is.
    let placed: Vec<(i64, Option<i64>)> = if geometry_models.is_empty() {
        geometries.keys().map(|&id| (id, None)).collect()
    } else {
        geometry_models
            .iter()
            .map(|&(geometry, model)| (geometry, Some(model)))
            .collect()
    };
    for (geometry, model) in placed {
        let (positions, triangles) = &geometries[&geometry];
        let start = mesh.positions.len() as u32;
        let matrix = model.map(world);
        mesh.positions.extend(
            positions
                .iter()
                .map(|&p| matrix.as_ref().map_or(p, |m| apply(m, p))),
        );
        let color = model
            .and_then(|model| model_color.get(&model).copied())
            .unwrap_or(PLAIN);
        let count = positions.len() as u32;
        for triangle in triangles {
            if triangle.iter().all(|&index| index < count) {
                mesh.triangles.push(triangle.map(|index| start + index));
                mesh.colors.push(color);
            }
        }
    }
    Ok(mesh)
}

/// FBX polygons as triangles: a polygon's last index is stored as its
/// bitwise not, so a negative entry closes it; each is cut into a fan.
fn fan_triangulate(indices: &[i32]) -> Vec<[u32; 3]> {
    let mut triangles = Vec::new();
    let mut polygon: Vec<u32> = Vec::new();
    for &raw in indices {
        let (index, last) = if raw < 0 { (!raw, true) } else { (raw, false) };
        polygon.push(index as u32);
        if last {
            for pair in 1..polygon.len().saturating_sub(1) {
                triangles.push([polygon[0], polygon[pair], polygon[pair + 1]]);
            }
            polygon.clear();
        }
    }
    triangles
}

fn translate([x, y, z]: [f64; 3]) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x as f32, y as f32, z as f32, 1.0],
    ]
}

fn scale([x, y, z]: [f64; 3]) -> [[f32; 4]; 4] {
    [
        [x as f32, 0.0, 0.0, 0.0],
        [0.0, y as f32, 0.0, 0.0],
        [0.0, 0.0, z as f32, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// FBX's default rotation order, degrees: X first, then Y, then Z.
fn euler_xyz([x, y, z]: [f64; 3]) -> [[f32; 4]; 4] {
    let (sx, cx) = (x.to_radians() as f32).sin_cos();
    let (sy, cy) = (y.to_radians() as f32).sin_cos();
    let (sz, cz) = (z.to_radians() as f32).sin_cos();
    let rx = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, cx, sx, 0.0],
        [0.0, -sx, cx, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let ry = [
        [cy, 0.0, -sy, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [sy, 0.0, cy, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let rz = [
        [cz, sz, 0.0, 0.0],
        [-sz, cz, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    multiply(&rz, &multiply(&ry, &rx))
}

// Column-major 4x4 product, as glTF stores its matrices.
fn multiply(a: &[[f32; 4]; 4], b: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0; 4]; 4];
    for (column, out_column) in out.iter_mut().enumerate() {
        for (row, cell) in out_column.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[k][row] * b[column][k]).sum();
        }
    }
    out
}

fn apply(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    let at = |row: usize| m[0][row] * p[0] + m[1][row] * p[1] + m[2][row] * p[2] + m[3][row];
    [at(0), at(1), at(2)]
}

/// Centres the model on its bounding box and scales it into the unit sphere.
fn normalize(positions: &mut [[f32; 3]]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for point in positions.iter() {
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    let center = [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ];
    let radius = positions
        .iter()
        .map(|p| {
            ((p[0] - center[0]).powi(2) + (p[1] - center[1]).powi(2) + (p[2] - center[2]).powi(2))
                .sqrt()
        })
        .fold(0.0f32, f32::max);
    let scale = if radius > 0.0 { 1.0 / radius } else { 1.0 };
    for point in positions.iter_mut() {
        for axis in 0..3 {
            point[axis] = (point[axis] - center[axis]) * scale;
        }
    }
}

/// Distance from the camera to the model's centre, in model radii: close
/// enough for some perspective, far enough that nothing reaches the camera.
const CAMERA: f32 = 3.0;

/// Draws `mesh` turned by `yaw` and `pitch` into a `width` x `height` BGRA
/// image, background transparent so the pane's backdrop shows through.
pub(crate) fn rasterize(
    mesh: &Mesh,
    yaw: f32,
    pitch: f32,
    zoom: f32,
    width: u32,
    height: u32,
) -> Option<RenderImage> {
    let (w, h) = (width as usize, height as usize);
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    // Fills about four fifths of the shorter side at zoom 1.
    let focal = 0.4 * width.min(height) as f32 * CAMERA * zoom;
    let (half_w, half_h) = (w as f32 / 2.0, h as f32 / 2.0);
    // Turned into view space, then projected: x right, y down, depth = the
    // distance from the camera, smaller nearer.
    let view: Vec<[f32; 3]> = mesh
        .positions
        .iter()
        .map(|&[x, y, z]| {
            let (x, z) = (x * cy + z * sy, -x * sy + z * cy);
            let (y, z) = (y * cp - z * sp, y * sp + z * cp);
            [x, y, z]
        })
        .collect();
    let screen: Vec<[f32; 3]> = view
        .iter()
        .map(|&[x, y, z]| {
            let depth = CAMERA - z;
            [
                half_w + x * focal / depth,
                half_h - y * focal / depth,
                depth,
            ]
        })
        .collect();
    let light = {
        let [x, y, z] = [-0.35f32, 0.55, 0.75];
        let length = (x * x + y * y + z * z).sqrt();
        [x / length, y / length, z / length]
    };
    // Each triangle's shaded colour and the rows it covers, worked out once.
    let shaded: Vec<Option<([u8; 3], usize, usize)>> = mesh
        .triangles
        .iter()
        .zip(&mesh.colors)
        .map(|(&[a, b, c], color)| {
            let (pa, pb, pc) = (view[a as usize], view[b as usize], view[c as usize]);
            let u = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
            let v = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if length == 0.0 {
                return None;
            }
            // Both sides lit alike: many models have faces wound either way.
            let lambert = ((n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / length).abs();
            let shade = 0.28 + 0.72 * lambert;
            let tint = color.map(|channel| (channel as f32 * shade).min(255.0) as u8);
            let ys = [
                screen[a as usize][1],
                screen[b as usize][1],
                screen[c as usize][1],
            ];
            let top = ys
                .iter()
                .copied()
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(0.0) as usize;
            let bottom = (ys
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .max(0.0) as usize)
                .min(h);
            (top < bottom).then_some((tint, top, bottom))
        })
        .collect();

    let mut pixels = vec![0u8; w * h * 4];
    let bands = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let band_rows = h.div_ceil(bands);
    std::thread::scope(|scope| {
        for (band, chunk) in pixels.chunks_mut(band_rows * w * 4).enumerate() {
            let (shaded, screen, triangles) = (&shaded, &screen, &mesh.triangles);
            scope.spawn(move || {
                let first = band * band_rows;
                let rows = chunk.len() / (w * 4);
                let mut depth = vec![f32::INFINITY; rows * w];
                for (triangle, info) in triangles.iter().zip(shaded) {
                    let Some((tint, top, bottom)) = *info else {
                        continue;
                    };
                    let (top, bottom) = (top.max(first), bottom.min(first + rows));
                    if top >= bottom {
                        continue;
                    }
                    let [a, b, c] = triangle.map(|index| screen[index as usize]);
                    let area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
                    if area.abs() < 1e-6 {
                        continue;
                    }
                    let left = (a[0].min(b[0]).min(c[0]).floor().max(0.0) as usize).min(w);
                    let right = (a[0].max(b[0]).max(c[0]).ceil().max(0.0) as usize).min(w);
                    for y in top..bottom {
                        let py = y as f32 + 0.5;
                        for x in left..right {
                            let px = x as f32 + 0.5;
                            let w0 = ((b[0] - px) * (c[1] - py) - (b[1] - py) * (c[0] - px)) / area;
                            let w1 = ((c[0] - px) * (a[1] - py) - (c[1] - py) * (a[0] - px)) / area;
                            let w2 = 1.0 - w0 - w1;
                            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                                continue;
                            }
                            let z = w0 * a[2] + w1 * b[2] + w2 * c[2];
                            let at = (y - first) * w + x;
                            if z < depth[at] {
                                depth[at] = z;
                                let [r, g, b] = tint;
                                chunk[at * 4..at * 4 + 4].copy_from_slice(&[b, g, r, 255]);
                            }
                        }
                    }
                }
            });
        }
    });
    super::thumbnails::render_image(width, height, pixels)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn cube_stl() -> &'static str {
        "solid c\n\
         facet normal 0 0 1\n outer loop\n vertex 0 0 1\n vertex 1 0 1\n vertex 1 1 1\n endloop\n endfacet\n\
         facet normal 0 0 1\n outer loop\n vertex 0 0 1\n vertex 1 1 1\n vertex 0 1 1\n endloop\n endfacet\n\
         facet normal 0 0 -1\n outer loop\n vertex 0 0 0\n vertex 1 1 0\n vertex 1 0 0\n endloop\n endfacet\n\
         endsolid c\n"
    }

    #[test]
    fn an_stl_loads_centred_in_the_unit_sphere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.stl");
        std::fs::write(&path, cube_stl()).unwrap();
        let mesh = load(path.to_str().unwrap()).unwrap();
        assert_eq!(mesh.triangles.len(), 3);
        let radius = mesh
            .positions
            .iter()
            .map(|p| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt())
            .fold(0.0, f32::max);
        assert!((radius - 1.0).abs() < 1e-4);
    }

    #[test]
    fn an_obj_takes_its_material_colour() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.mtl"), "newmtl red\nKd 1 0 0\n").unwrap();
        let path = dir.path().join("t.obj");
        std::fs::write(
            &path,
            "mtllib m.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nv 0 0 1\nusemtl red\nf 1 2 3 4\n",
        )
        .unwrap();
        let mesh = load(path.to_str().unwrap()).unwrap();
        // The quad, triangulated.
        assert_eq!(mesh.triangles.len(), 2);
        assert_eq!(mesh.colors[0], [255, 0, 0]);
    }

    #[test]
    fn the_model_covers_the_middle_and_leaves_the_corners_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.stl");
        std::fs::write(&path, cube_stl()).unwrap();
        let mesh = load(path.to_str().unwrap()).unwrap();
        let (w, h) = (64usize, 48usize);
        let pixels = rasterize_pixels(&mesh, 0.0, 0.0, 1.0, w as u32, h as u32);
        let alpha = |x: usize, y: usize| pixels[(y * w + x) * 4 + 3];
        assert_eq!(alpha(w / 2, h / 2), 255);
        assert_eq!(alpha(0, 0), 0);
    }

    #[test]
    fn fbx_polygons_close_on_a_negative_index() {
        // A quad (0 1 2 ~3) and a triangle (3 4 ~5).
        assert_eq!(
            fan_triangulate(&[0, 1, 2, !3, 3, 4, !5]),
            [[0, 1, 2], [0, 2, 3], [3, 4, 5]]
        );
    }

    #[test]
    fn euler_rotation_turns_x_into_y_about_z() {
        let point = apply(&euler_xyz([0.0, 0.0, 90.0]), [1.0, 0.0, 0.0]);
        assert!((point[0]).abs() < 1e-5 && (point[1] - 1.0).abs() < 1e-5);
    }

    #[test]
    #[ignore = "reads a model from the owner's Downloads"]
    fn the_owners_fbx_loads() {
        let home = std::env::var("USERPROFILE").unwrap();
        let mesh = load(&format!("{home}/Downloads/model coelho foda.fbx")).unwrap();
        assert!(!mesh.triangles.is_empty());
        let pixels = rasterize_pixels(&mesh, -0.6, 0.35, 1.0, 320, 240);
        let covered = pixels.chunks_exact(4).filter(|p| p[3] == 255).count();
        eprintln!(
            "{} triangles, {covered} pixels covered",
            mesh.triangles.len()
        );
        assert!(covered > 1000);
    }

    // The pixels `rasterize` would hand gpui, read back for the test.
    fn rasterize_pixels(mesh: &Mesh, yaw: f32, pitch: f32, zoom: f32, w: u32, h: u32) -> Vec<u8> {
        let image = rasterize(mesh, yaw, pitch, zoom, w, h).unwrap();
        image.as_bytes(0).unwrap().to_vec()
    }
}
