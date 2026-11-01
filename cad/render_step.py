"""Render a STEP assembly with its colours to PNGs from five angles.

usage: render_step.py <model.step> <outdir>

Writes <outdir>/<model>_{iso_front,iso_back,top,bottom,side}.png: RGBA with
a transparent background, cropped to the model plus a margin. OpenCascade
(OCP) reads and meshes the model, PyVista renders it off screen.
"""
import os
import sys
import time

import numpy as np
import pyvista as pv
from PIL import Image
from OCP.BRep import BRep_Tool
from OCP.BRepMesh import BRepMesh_IncrementalMesh
from OCP.OCP.collections import Sequence_TDF_Label
from OCP.Quantity import Quantity_Color
from OCP.STEPCAFControl import STEPCAFControl_Reader
from OCP.TCollection import TCollection_ExtendedString
from OCP.TDF import TDF_Label
from OCP.TDocStd import TDocStd_Document
from OCP.TopAbs import TopAbs_FACE, TopAbs_REVERSED
from OCP.TopExp import TopExp_Explorer
from OCP.TopLoc import TopLoc_Location
from OCP.TopoDS import TopoDS
from OCP.XCAFDoc import (XCAFDoc_ColorTool, XCAFDoc_ColorType,
                         XCAFDoc_DocumentTool, XCAFDoc_ShapeTool)

# Mesh tolerances: linear deflection in mm, angular in radians.
DEFLECTION = 0.02
ANGLE = 0.3

SIZE = (1920, 1440)  # render size before cropping
MARGIN = 40          # pixels kept around the model when cropping
DEFAULT = (0.6, 0.6, 0.6)
COLOR_TYPES = (XCAFDoc_ColorType.XCAFDoc_ColorSurf, XCAFDoc_ColorType.XCAFDoc_ColorGen)

# The views: the direction from the model to the camera, and the up vector.
VIEWS = {
    "iso_front": ((-1, -1.4, 1.1), (0, 0, 1)),
    "iso_back": ((1.2, 1.4, 1.0), (0, 0, 1)),
    "top": ((0, 0, 1), (0, 1, 0)),
    "bottom": ((0, 0, -1), (0, 1, 0)),
    "side": ((0, -1, 0.18), (0, 0, 1)),
}
ORTHO = ("top", "bottom")


def read(path):
    """Read the STEP file with its assembly structure and colours."""
    doc = TDocStd_Document(TCollection_ExtendedString("doc"))
    r = STEPCAFControl_Reader()
    r.SetColorMode(True)
    r.SetNameMode(True)
    if r.ReadFile(path) != 1:
        sys.exit(f"render_step: cannot read {path}")
    r.Transfer(doc)
    return doc


class Mesher:
    """Collects the triangles of the assembly with a colour per triangle."""

    def __init__(self, doc):
        self.colors = XCAFDoc_DocumentTool.ColorTool_s(doc.Main())
        self.pts, self.tris, self.cols = [], [], []
        self.npts = 0
        free = Sequence_TDF_Label()
        XCAFDoc_DocumentTool.ShapeTool_s(doc.Main()).GetFreeShapes(free)
        for label in free:
            self.walk(label, TopLoc_Location(), None)

    @staticmethod
    def label_color(label):
        c = Quantity_Color()
        for t in COLOR_TYPES:
            if XCAFDoc_ColorTool.GetColor_s(label, t, c):
                return (c.Red(), c.Green(), c.Blue())
        return None

    def face_color(self, face):
        c = Quantity_Color()
        for t in COLOR_TYPES:
            if self.colors.GetColor(face, t, c):
                return (c.Red(), c.Green(), c.Blue())
        return None

    def walk(self, label, loc, color):
        """Descends the assembly; instance colours override part colours."""
        color = self.label_color(label) or color
        if XCAFDoc_ShapeTool.IsReference_s(label):
            ref = TDF_Label()
            XCAFDoc_ShapeTool.GetReferredShape_s(label, ref)
            self.walk(ref, loc.Multiplied(XCAFDoc_ShapeTool.GetLocation_s(label)), color)
        elif XCAFDoc_ShapeTool.IsAssembly_s(label):
            parts = Sequence_TDF_Label()
            XCAFDoc_ShapeTool.GetComponents_s(label, parts)
            for part in parts:
                self.walk(part, loc, color)
        else:
            self.add(XCAFDoc_ShapeTool.GetShape_s(label), loc, color or DEFAULT)

    def add(self, shape, loc, color):
        # Meshing a shape that already has a triangulation does nothing.
        BRepMesh_IncrementalMesh(shape, DEFLECTION, False, ANGLE, True)
        exp = TopExp_Explorer(shape, TopAbs_FACE)
        while exp.More():
            face = TopoDS.Face(exp.Current())
            floc = TopLoc_Location()
            tri = BRep_Tool.Triangulation_s(face, floc)
            if tri is not None:
                self.add_face(face, tri, loc.Multiplied(floc).Transformation(),
                              self.face_color(face) or color)
            exp.Next()

    def add_face(self, face, tri, trsf, color):
        n, m = tri.NbNodes(), tri.NbTriangles()
        p = np.empty((n, 3))
        for i in range(n):
            q = tri.Node(i + 1).Transformed(trsf)
            p[i] = (q.X(), q.Y(), q.Z())
        t = np.empty((m, 3), dtype=np.int64)
        reversed_ = face.Orientation() == TopAbs_REVERSED
        for i in range(m):
            a, b, c = tri.Triangle(i + 1).Get()
            t[i] = (a, c, b) if reversed_ else (a, b, c)
        self.pts.append(p)
        self.tris.append(t - 1 + self.npts)
        self.cols.append(np.tile(color, (m, 1)))
        self.npts += n

    def polydata(self):
        tris = np.vstack(self.tris)
        faces = np.hstack([np.full((len(tris), 1), 3), tris]).ravel()
        mesh = pv.PolyData(np.vstack(self.pts), faces)
        mesh.cell_data["rgb"] = (np.vstack(self.cols) * 255).astype(np.uint8)
        return mesh


def render(mesh, name, direction, up, out):
    center = np.array(mesh.center)
    b = mesh.bounds
    size = np.linalg.norm(np.array(b[1::2]) - np.array(b[0::2]))
    d = np.array(direction, float) / np.linalg.norm(direction)

    pl = pv.Plotter(off_screen=True, window_size=SIZE)
    pl.add_mesh(mesh, scalars="rgb", rgb=True, show_scalar_bar=False,
                smooth_shading=False, specular=0.3, specular_power=20)
    if name in ORTHO:
        pl.enable_parallel_projection()
    pl.camera.up = up
    pl.reset_camera(bounds=b)
    pl.camera.focal_point = center
    pl.camera.position = center + d * size * 2
    if name in ORTHO:
        pl.camera.parallel_scale = 0.55 * (b[3] - b[2])
    else:
        pl.camera.zoom(1.3 if name == "side" else 1.15)
    pl.enable_anti_aliasing("ssaa")
    img = pl.screenshot(None, transparent_background=True, return_img=True)
    pl.close()

    im = Image.fromarray(img)
    l, t, r, btm = im.getchannel("A").getbbox()
    im = im.crop((max(l - MARGIN, 0), max(t - MARGIN, 0),
                  min(r + MARGIN, im.width), min(btm + MARGIN, im.height)))
    im.save(out, optimize=True)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.split("\n\n")[1])
    path, outdir = sys.argv[1], sys.argv[2]
    base = os.path.splitext(os.path.basename(path))[0]
    t0 = time.time()
    mesh = Mesher(read(path)).polydata()
    print(f"{path}: {mesh.n_cells} triangles in {time.time() - t0:.0f} s", flush=True)
    for name, (direction, up) in VIEWS.items():
        out = os.path.join(outdir, f"{base}_{name}.png")
        render(mesh, name, direction, up, out)
        print("wrote", out, flush=True)


if __name__ == "__main__":
    main()
