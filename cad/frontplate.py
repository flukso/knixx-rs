"""Build the Thunder front plate and, with the boards' model, the assembly.

usage: frontplate.py <outdir> <name> [<model.step>]

Writes, from the dimensions below alone:
  <outdir>/<name>_frontplate.step   the plate alone, in mm, with its
                                    lower-left corner at the origin and its
                                    underside at z = 0
  <outdir>/<name>_frontplate.dxf    the plate's outline for cutting, in the
                                    same frame
  <outdir>/<name>_frontplate_<curve>.dxf
                                    the outline with a curve of CURVES
                                    engraved, one DXF for each
  <outdir>/<name>_sampler.dxf       the curves of SAMPLER engraved in a
                                    3 x 3 grid on a square of the plate's
                                    size, without holes
  <outdir>/<name>_sampler.png       the sampler drawn in grey on a
                                    transparent background

With the STEP model of the boards, it first checks the plate against it:
the switch spacing against the holes and the VL53L7CX against the slot.
It then also writes:
  <outdir>/<name>_assembly.step     the boards, with the sensor board moved
                                    to its standoff height, plus the plate
"""

import cmath
import math
import os
import sys

from OCP.BRepAdaptor import BRepAdaptor_Curve, BRepAdaptor_Surface
from OCP.BRepAlgoAPI import BRepAlgoAPI_Cut
from OCP.BRepBuilderAPI import BRepBuilderAPI_Transform
from OCP.BRepBndLib import BRepBndLib
from OCP.BRepGProp import BRepGProp
from OCP.BRepPrimAPI import BRepPrimAPI_MakeBox, BRepPrimAPI_MakeCylinder
from OCP.Bnd import Bnd_Box
from OCP.GProp import GProp_GProps
from OCP.IFSelect import IFSelect_RetDone
from OCP.Interface import Interface_Static
from OCP.Quantity import Quantity_Color, Quantity_TOC_RGB
from OCP.STEPCAFControl import STEPCAFControl_Reader, STEPCAFControl_Writer
from OCP.GeomAbs import GeomAbs_Circle, GeomAbs_Line, GeomAbs_Plane
from OCP.STEPControl import STEPControl_AsIs
from OCP.TCollection import TCollection_ExtendedString
from OCP.TDF import TDF_Label
from OCP.TDataStd import TDataStd_Name
from OCP.TDocStd import TDocStd_Document
from OCP.TopAbs import TopAbs_EDGE, TopAbs_FACE, TopAbs_SHELL
from OCP.TopExp import TopExp_Explorer
from OCP.TopLoc import TopLoc_Location
from OCP.TopoDS import TopoDS
from OCP.XCAFDoc import XCAFDoc_ColorSurf, XCAFDoc_DocumentTool, XCAFDoc_Location
from OCP.collections import Sequence_TDF_Label
from OCP.gp import gp_Ax2, gp_Dir, gp_Pnt, gp_Trsf, gp_Vec
from PIL import Image, ImageDraw

# Plate, in mm. X runs between the switches, +Y towards the sensor board.
PLATE_SIZE = 80.0
PLATE_THICKNESS = 1.0
HOLE_DIAMETER = 12.2  # clearance for the M12 switch bushings
HOLE_OFFSET = 20.0  # from the centre, left and right
CUTOUT_WIDTH = 20.0  # of the rectangle; the half-circle ends add CUTOUT_HEIGHT
CUTOUT_HEIGHT = 3.2
# The cutout is centred in X, and in Y on the VL53L7CX (U3 on the sensor
# board), towards which +Y points: SLOT_OFFSET above the line through the
# switch axes, as measured on the model. With the model, the script checks
# that the sensor is still there, within SLOT_TOLERANCE.
VL53 = "U3"
SLOT_OFFSET = 17.955
SLOT_TOLERANCE = 0.05
PLATE_GAP = 3.0  # from the top of the toggle switch bodies to the plate
PLATE_COLOR = (0.62, 0.64, 0.66)

# The engravings, on the DXF's ENGRAVE layer: each curve is one closed
# polyline, centred in X and a quarter of the plate up from its bottom edge,
# scaled so that its point farthest from the centre is CURVE_RADIUS away.
CURVE_RADIUS = 11.0
CURVE_POINTS = 3600


# The sampler's PNG: pixels per mm, line width in mm, line colour, and the
# supersampling that smooths its lines.
PNG_SCALE = 10
PNG_LINE = 0.3
PNG_COLOR = (128, 128, 128)
PNG_SUPERSAMPLE = 4

# The curves of the sampler, row by row from the top left.
SAMPLER = ("cassini", "trefoil", "teardrop",
           "nephroid", "lissajous", "limacon",
           "deltoid", "hypotrochoid", "bicorn")


def _polar(r):
    """Returns the curve of the polar equation r(theta)."""
    return lambda t: r(t) * cmath.exp(1j * t)


def _cassini(t, a=1.0, b=1.05):
    # One oval, dimpled, for a < b < a sqrt 2.
    c2 = math.cos(2 * t)
    return math.sqrt(a * a * c2 + math.sqrt(b ** 4 - a ** 4 * (1 - c2 * c2))) * cmath.exp(1j * t)


# The curves, by the name in their DXF's file name: z(t), t running over
# [0, period), and whether the curve is centred on the origin of z, its
# centre of symmetry, or else on the middle of its bounding box.
CURVES = {
    # R = 5, r = 3, d = 5: closes after 3 turns.
    "hypotrochoid": (lambda t: 2 * cmath.exp(1j * t) + 5 * cmath.exp(-2j * t / 3), 6 * math.pi, True),
    # Of Bernoulli.
    "lemniscate": (lambda t: complex(math.cos(t), math.sin(t) * math.cos(t)) / (1 + math.sin(t) ** 2),
                   2 * math.pi, True),
    # Frequencies 1:2, phase pi/4 on the second: on the first, the curve
    # folds onto the parabola y = 2x^2 - 1.
    "lissajous": (lambda t: complex(math.sin(t), math.sin(2 * t + math.pi / 4)), 2 * math.pi, True),
    "cardioid": (_polar(lambda t: 1 - math.cos(t)), 2 * math.pi, False),
    "astroid": (lambda t: complex(math.cos(t) ** 3, math.sin(t) ** 3), 2 * math.pi, True),
    "nephroid": (lambda t: 3 * cmath.exp(1j * t) - cmath.exp(3j * t), 2 * math.pi, True),
    "deltoid": (lambda t: 2 * cmath.exp(1j * t) + cmath.exp(-2j * t), 2 * math.pi, True),
    # With its inner loop.
    "limacon": (_polar(lambda t: 1 + 2 * math.cos(t)), 2 * math.pi, False),
    "cassini": (_cassini, 2 * math.pi, True),
    # The projection of the trefoil knot.
    "trefoil": (lambda t: complex(math.sin(t) + 2 * math.sin(2 * t), math.cos(t) - 2 * math.cos(2 * t)),
                2 * math.pi, True),
    # Frank Farris's, with frequencies 1, 6 and -14.
    "mystery": (lambda t: cmath.exp(1j * t) + cmath.exp(6j * t) / 2 + 1j * cmath.exp(-14j * t) / 3,
                2 * math.pi, True),
    "teardrop": (lambda t: complex(math.cos(t), math.sin(t) * math.sin(t / 2) ** 2), 2 * math.pi, False),
    "fish": (lambda t: complex(math.cos(t) - math.sin(t) ** 2 / math.sqrt(2), math.cos(t) * math.sin(t)),
             2 * math.pi, False),
    "bicorn": (lambda t: complex(math.sin(t), math.cos(t) ** 2 * (2 + math.cos(t)) / (3 + math.sin(t) ** 2)),
               2 * math.pi, False),
}

# The sensor board's underside sits this far above the main board's top.
SENSOR_STANDOFF = 12.0

TOLERANCE = 0.2  # allowed mismatch between switch spacing and hole spacing
BUSHING_DIAMETER = 12.0  # nominal, to tell the bushing from the other shells


def bbox(shape):
    b = Bnd_Box()
    BRepBndLib.Add_s(shape, b, True)
    p, q = b.CornerMin(), b.CornerMax()
    return p.X(), p.Y(), p.Z(), q.X(), q.Y(), q.Z()


def name(label):
    n = TDataStd_Name()
    if label.FindAttribute(TDataStd_Name.GetID_s(), n):
        return n.Get().ToExtString()
    return ""


class Model:
    def __init__(self, path):
        self.doc = TDocStd_Document(TCollection_ExtendedString("XmlOcaf"))
        r = STEPCAFControl_Reader()
        r.SetNameMode(True)
        r.SetColorMode(True)
        if r.ReadFile(path) != IFSelect_RetDone or not r.Transfer(self.doc):
            sys.exit(f"cannot read {path}")
        self.st = XCAFDoc_DocumentTool.ShapeTool_s(self.doc.Main())
        roots = Sequence_TDF_Label()
        self.st.GetFreeShapes(roots)
        if roots.Length() != 1:
            sys.exit(f"expected one root shape, found {roots.Length()}")
        self.root = roots.Value(1)

    def referred(self, label):
        if self.st.IsReference_s(label):
            ref = TDF_Label()
            self.st.GetReferredShape_s(label, ref)
            return ref
        return label

    def components(self, label):
        kids = Sequence_TDF_Label()
        self.st.GetComponents_s(self.referred(label), kids, False)
        return [kids.Value(i) for i in range(1, kids.Length() + 1)]

    def find(self, label, want, path=()):
        """Yields the component paths under label whose last name is want."""
        for c in self.components(label):
            p = path + (c,)
            if name(c) == want:
                yield p
            yield from self.find(c, want, p)

    def leaves(self, path):
        """Yields the paths of the parts under path."""
        kids = self.components(path[-1])
        if not kids:
            yield path
        for c in kids:
            yield from self.leaves(path + (c,))

    def world(self, path):
        """Returns the shape at the end of path placed in the root frame."""
        loc = TopLoc_Location()
        for c in path:
            loc = loc * self.st.GetLocation_s(c)
        return self.st.GetShape_s(self.referred(path[-1])).Moved(loc)


def switch_geometry(model, path):
    """Returns the bushing axis (x, y) and the body top z of a toggle switch.
    The body is the widest shell; the threaded bushing is the shell with a
    square footprint of about 12 mm that starts at the body top."""
    shells = []
    e = TopExp_Explorer(model.world(path), TopAbs_SHELL)
    while e.More():
        shells.append(bbox(e.Current()))
        e.Next()
    body = max(shells, key=lambda b: (b[3] - b[0]) * (b[4] - b[1]))
    bushings = [b for b in shells
                if abs((b[3] - b[0]) - BUSHING_DIAMETER) < 0.5
                and abs((b[4] - b[1]) - BUSHING_DIAMETER) < 0.5]
    if len(bushings) != 1:
        sys.exit(f"switch {name(path[-1])}: found {len(bushings)} bushings")
    b = bushings[0]
    return ((b[0] + b[3]) / 2, (b[1] + b[4]) / 2), body[5], b


def make_plate():
    """Returns the plate in its own frame: the lower-left corner at the
    origin, the underside at z = 0, the switch axes on y = PLATE_SIZE / 2."""
    s = PLATE_SIZE
    t = PLATE_THICKNESS
    cx = cy = s / 2
    cutout_y = cy + SLOT_OFFSET
    plate = BRepPrimAPI_MakeBox(gp_Pnt(0, 0, 0), s, s, t).Shape()
    for dx in (-HOLE_OFFSET, HOLE_OFFSET):
        hole = BRepPrimAPI_MakeCylinder(
            gp_Ax2(gp_Pnt(cx + dx, cy, -1), gp_Dir(0, 0, 1)),
            HOLE_DIAMETER / 2, t + 2).Shape()
        plate = BRepAlgoAPI_Cut(plate, hole).Shape()
    cutout = BRepPrimAPI_MakeBox(
        gp_Pnt(cx - CUTOUT_WIDTH / 2, cutout_y - CUTOUT_HEIGHT / 2, -1),
        CUTOUT_WIDTH, CUTOUT_HEIGHT, t + 2).Shape()
    plate = BRepAlgoAPI_Cut(plate, cutout).Shape()
    # Half-circles on both ends make the cutout a slot.
    for dx in (-CUTOUT_WIDTH / 2, CUTOUT_WIDTH / 2):
        end = BRepPrimAPI_MakeCylinder(
            gp_Ax2(gp_Pnt(cx + dx, cutout_y, -1), gp_Dir(0, 0, 1)),
            CUTOUT_HEIGHT / 2, t + 2).Shape()
        plate = BRepAlgoAPI_Cut(plate, end).Shape()
    return plate


def placed(plate, cx, cy, z):
    """Returns plate moved from its own frame to the switch axes' centre
    (cx, cy) in the model, with its underside at z."""
    t = gp_Trsf()
    t.SetTranslation(gp_Vec(cx - PLATE_SIZE / 2, cy - PLATE_SIZE / 2, z))
    return BRepBuilderAPI_Transform(plate, t, True).Shape()


def add_plate(doc, st, plate, parent=None):
    """Adds the plate to doc, as a component of parent if given."""
    proto = st.AddShape(plate, False)
    TDataStd_Name.Set_s(proto, TCollection_ExtendedString("Front plate"))
    ct = XCAFDoc_DocumentTool.ColorTool_s(doc.Main())
    ct.SetColor(proto, Quantity_Color(*PLATE_COLOR, Quantity_TOC_RGB), XCAFDoc_ColorSurf)
    if parent is not None:
        comp = st.AddComponent(parent, proto, TopLoc_Location())
        TDataStd_Name.Set_s(comp, TCollection_ExtendedString("Front plate"))
    st.UpdateAssemblies()


def outline(plate):
    """Returns the edges of the plate's top face as DXF entities in mm:
    lines, circles and arcs, shifted so that the plate's lower-left corner
    is at the origin."""
    x0, y0, _, _, _, z1 = bbox(plate)
    top = None
    e = TopExp_Explorer(plate, TopAbs_FACE)
    while e.More():
        f = TopoDS.Face(e.Current())
        if BRepAdaptor_Surface(f).GetType() == GeomAbs_Plane and abs(bbox(f)[2] - z1) < 1e-6:
            top = f
        e.Next()
    if top is None:
        sys.exit("plate: no top face")

    def pt(p):
        return p.X() - x0, p.Y() - y0

    entities = []
    e = TopExp_Explorer(top, TopAbs_EDGE)
    while e.More():
        c = BRepAdaptor_Curve(TopoDS.Edge(e.Current()))
        t0, t1 = c.FirstParameter(), c.LastParameter()
        if c.GetType() == GeomAbs_Line:
            (ax, ay), (bx, by) = pt(c.Value(t0)), pt(c.Value(t1))
            entities.append(("LINE", [(10, ax), (20, ay), (11, bx), (21, by)]))
        elif c.GetType() == GeomAbs_Circle:
            circ = c.Circle()
            cx, cy = pt(circ.Location())
            r = circ.Radius()
            if abs(t1 - t0 - 2 * math.pi) < 1e-9:
                entities.append(("CIRCLE", [(10, cx), (20, cy), (40, r)]))
            else:
                # DXF arcs run counterclockwise from the start angle.
                angles = [math.degrees(math.atan2(y - cy, x - cx)) % 360
                          for x, y in (pt(c.Value(t0)), pt(c.Value(t1)))]
                if circ.Axis().Direction().Z() < 0:
                    angles.reverse()
                entities.append(("ARC", [(10, cx), (20, cy), (40, r), (50, angles[0]), (51, angles[1])]))
        else:
            sys.exit(f"plate: cannot write a {c.GetType()} edge to DXF")
        e.Next()

    return entities


def sampler_centres():
    """Returns the curves of SAMPLER with the centres of their cells."""
    s = PLATE_SIZE
    return [(curve, s * (2 * (i % 3) + 1) / 6, s * (5 - 2 * (i // 3)) / 6)
            for i, curve in enumerate(SAMPLER)]


def write_sampler(path):
    """Writes the sampler as an R12 DXF: the square of the plate on layer 0,
    and the curves of SAMPLER in the centres of a 3 x 3 grid on it."""
    s = PLATE_SIZE
    out = ["0", "SECTION", "2", "ENTITIES"]
    for (ax, ay), (bx, by) in (((0, 0), (s, 0)), ((s, 0), (s, s)), ((s, s), (0, s)), ((0, s), (0, 0))):
        out += ["0", "LINE", "8", "0", "10", f"{ax:.6f}", "20", f"{ay:.6f}", "11", f"{bx:.6f}", "21", f"{by:.6f}"]
    for curve, cx, cy in sampler_centres():
        out += engraving(curve, cx, cy)
    out += ["0", "ENDSEC", "0", "EOF"]
    with open(path, "w") as f:
        f.write("\n".join(out) + "\n")


def write_sampler_png(path):
    """Draws the sampler, the square and its curves, in PNG_COLOR on a
    transparent background. The lines are drawn into an alpha mask at
    PNG_SUPERSAMPLE times the size, which is then scaled down."""
    k = PNG_SCALE * PNG_SUPERSAMPLE
    width = round(PNG_LINE * k)
    pad = width  # keeps the square's outer half-width in the image
    size = round(PLATE_SIZE * k) + 2 * pad

    def px(x, y):
        return pad + x * k, size - pad - y * k

    mask = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(mask)
    s = PLATE_SIZE
    square = [px(0, 0), px(s, 0), px(s, s), px(0, s)]
    draw.line(square + square[:1], fill=255, width=width, joint="curve")
    for curve, cx, cy in sampler_centres():
        pts = [px(cx + p.real, cy + p.imag) for p in curve_points(curve, CURVE_POINTS)]
        draw.line(pts + pts[:1], fill=255, width=width, joint="curve")
    mask = mask.resize((size // PNG_SUPERSAMPLE,) * 2, Image.LANCZOS)
    img = Image.new("RGBA", mask.size, PNG_COLOR + (0,))
    img.putalpha(mask)
    img.save(path, optimize=True)


def write_dxf(entities, path, curve=None):
    """Writes entities, from outline, as an R12 DXF on layer 0, with the
    curve of CURVES named curve engraved."""
    out = ["0", "SECTION", "2", "ENTITIES"]
    for kind, codes in entities:
        out += ["0", kind, "8", "0"]
        for code, v in codes:
            out += [str(code), f"{v:.6f}"]
    if curve is not None:
        out += engraving(curve, PLATE_SIZE / 2, PLATE_SIZE / 4)
    out += ["0", "ENDSEC", "0", "EOF"]
    with open(path, "w") as f:
        f.write("\n".join(out) + "\n")


def curve_points(curve, n):
    """Returns n points of the curve of CURVES named curve, around its
    centre, scaled to CURVE_RADIUS."""
    z, period, symmetric = CURVES[curve]
    pts = [z(period * k / n) for k in range(n)]
    if not symmetric:
        xs, ys = [p.real for p in pts], [p.imag for p in pts]
        c = complex(min(xs) + max(xs), min(ys) + max(ys)) / 2
        pts = [p - c for p in pts]
    scale = CURVE_RADIUS / max(abs(p) for p in pts)
    return [scale * p for p in pts]


def engraving(curve, cx, cy):
    """Returns the DXF entities of the engraving of the curve of CURVES named
    curve, centred on (cx, cy), on layer ENGRAVE: one closed POLYLINE, so
    that it is a single toolpath."""
    pts = curve_points(curve, CURVE_POINTS)
    out = ["0", "POLYLINE", "8", "ENGRAVE", "66", "1", "10", "0.0", "20", "0.0", "30", "0.0", "70", "1"]
    for p in pts:
        out += ["0", "VERTEX", "8", "ENGRAVE",
                "10", f"{cx + p.real:.6f}", "20", f"{cy + p.imag:.6f}", "30", "0.0"]
    out += ["0", "SEQEND", "8", "ENGRAVE"]
    return out


def write(doc, path):
    Interface_Static.SetCVal_s("write.step.schema", "AP214IS")
    Interface_Static.SetCVal_s("write.step.unit", "MM")
    w = STEPCAFControl_Writer()
    w.SetNameMode(True)
    w.SetColorMode(True)
    if not w.Transfer(doc, STEPControl_AsIs) or w.Write(path) != IFSelect_RetDone:
        sys.exit(f"cannot write {path}")


def fit(model):
    """Checks the plate against the STEP model of the boards and returns the
    model, with the sensor board moved to its standoff height, and where the
    plate goes in it: the centre of the switch axes and the underside's z."""
    m = Model(model)

    # The main board is the component holding the switches; the other
    # top-level component is the sensor board.
    top = m.components(m.root)
    switches = [p for want in ("SW1", "SW2") for p in m.find(m.root, want)]
    if len(top) != 2 or len(switches) != 2:
        sys.exit(f"expected 2 top-level components and 2 switches, found {len(top)} and {len(switches)}")
    main_board = switches[0][0]
    sensor = next(c for c in top if not c.IsEqual(main_board))

    # The board itself is the flat plate spanning the assembly at z <= 0.
    board = next(p for p in m.find(main_board, "1"))
    board_top = bbox(m.world(board))[5]
    # The sensor board itself is the part with the largest footprint.
    def area(p):
        x0, y0, _, x1, y1, _ = bbox(m.world(p))
        return (x1 - x0) * (y1 - y0)
    sensor_board = max(m.leaves((sensor,)), key=area)
    sensor_bottom = bbox(m.world(sensor_board))[2]
    lift = board_top + SENSOR_STANDOFF - sensor_bottom
    t = gp_Trsf()
    t.SetTranslation(gp_Vec(0, 0, lift))
    XCAFDoc_Location.Set_s(sensor, TopLoc_Location(t) * m.st.GetLocation_s(sensor))
    m.st.UpdateAssemblies()
    print(f"main board top z={board_top:.2f}; sensor board underside "
          f"z={sensor_bottom:.2f} -> {sensor_bottom + lift:.2f} (moved {lift:+.2f})")

    geo = sorted(switch_geometry(m, p) for p in switches)
    (a, body_a, _), (b, body_b, _) = geo
    spacing = math.dist(a, b)
    if abs(spacing - 2 * HOLE_OFFSET) > TOLERANCE or abs(a[1] - b[1]) > TOLERANCE:
        sys.exit(f"switch axes {a} and {b}: {spacing:.2f} mm apart, holes are {2 * HOLE_OFFSET} mm")
    cx, cy = (a[0] + b[0]) / 2, (a[1] + b[1]) / 2
    z = max(body_a, body_b) + PLATE_GAP
    print(f"switch axes at ({a[0]:.2f}, {a[1]:.2f}) and ({b[0]:.2f}, {b[1]:.2f}); "
          f"body tops z={body_a:.2f}, {body_b:.2f}; plate underside z={z:.2f}")

    vl53 = [p for p in m.find(sensor, VL53)]
    if len(vl53) != 1:
        sys.exit(f"expected one {VL53} on the sensor board, found {len(vl53)}")
    _, y0, _, _, y1, _ = bbox(m.world((sensor,) + vl53[0]))
    offset = (y0 + y1) / 2 - cy
    print(f"VL53L7CX centre {offset:.3f} mm from the switch axis line")
    if abs(offset - SLOT_OFFSET) > SLOT_TOLERANCE:
        sys.exit(f"the VL53L7CX is {offset:.3f} mm from the switch axis line, the slot "
                 f"{SLOT_OFFSET} mm: set SLOT_OFFSET")
    return m, cx, cy, z


def main():
    if len(sys.argv) not in (3, 4):
        sys.exit("usage: frontplate.py <outdir> <name> [<model.step>]")
    outdir, name = sys.argv[1:3]
    model = sys.argv[3] if len(sys.argv) == 4 else None
    base = os.path.join(outdir, name)

    # Check the model first, so that a mismatch writes nothing.
    fitted = fit(model) if model else None

    plate = make_plate()
    plate_out = base + "_frontplate.step"
    pdoc = TDocStd_Document(TCollection_ExtendedString("XmlOcaf"))
    add_plate(pdoc, XCAFDoc_DocumentTool.ShapeTool_s(pdoc.Main()), plate)
    write(pdoc, plate_out)
    entities = outline(plate)
    outs = [plate_out, base + "_frontplate.dxf"]
    write_dxf(entities, outs[-1])
    for curve in CURVES:
        outs.append(f"{base}_frontplate_{curve}.dxf")
        write_dxf(entities, outs[-1], curve)
    outs.append(base + "_sampler.dxf")
    write_sampler(outs[-1])
    outs.append(base + "_sampler.png")
    write_sampler_png(outs[-1])

    if fitted:
        m, cx, cy, z = fitted
        add_plate(m.doc, m.st, placed(plate, cx, cy, z), parent=m.root)
        outs.append(base + "_assembly.step")
        write(m.doc, outs[-1])

    props = GProp_GProps()
    BRepGProp.VolumeProperties_s(plate, props)
    print(f"plate bbox {tuple(round(v, 2) for v in bbox(plate))}, volume {props.Mass():.1f} mm3")
    print(f"dxf: {', '.join(k for k, _ in entities)}")
    for out in outs:
        print(f"wrote {out}")


if __name__ == "__main__":
    main()
