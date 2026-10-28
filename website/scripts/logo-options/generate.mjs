// Generates logo explorations for Maze: static SVGs (tile + bare glyph) and a gallery page.
// Usage: node scripts/logo-options/generate.mjs  ->  scripts/logo-options/out/
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const out = join(dirname(fileURLToPath(import.meta.url)), "out");
mkdirSync(out, { recursive: true });

const W = 48; // stroke width on the 1024 canvas
const AMBER = "#FFB86B";

// Polyline with every corner rounded by radius r (clamped to half the shorter segment).
function rounded(points, r = 40, closed = false) {
  const n = points.length;
  const m = closed ? [(points[n - 1][0] + points[0][0]) / 2, (points[n - 1][1] + points[0][1]) / 2] : null;
  const p = closed ? [m, ...points, m] : points;
  let d = `M${f(p[0][0])} ${f(p[0][1])}`;
  const start = 1;
  const end = p.length - 1;
  for (let i = start; i < end; i++) {
    const [ax, ay] = p[i - 1], [bx, by] = p[i], [cx, cy] = p[i + 1];
    const l1 = Math.hypot(bx - ax, by - ay), l2 = Math.hypot(cx - bx, cy - by);
    const rr = Math.min(r, l1 / 2, l2 / 2);
    const x1 = bx - ((bx - ax) / l1) * rr, y1 = by - ((by - ay) / l1) * rr;
    const x2 = bx + ((cx - bx) / l2) * rr, y2 = by + ((cy - by) / l2) * rr;
    d += ` L${f(x1)} ${f(y1)} Q${bx} ${by} ${f(x2)} ${f(y2)}`;
  }
  d += closed ? " Z" : ` L${f(p.at(-1)[0])} ${f(p.at(-1)[1])}`;
  return d;
}
const f = (n) => Math.round(n * 10) / 10;
const path = (d, extra = "") => `<path d="${d}"${extra}/>`;
const dot = (cx, cy, r = 38) => `<circle cx="${cx}" cy="${cy}" r="${r}" fill="${AMBER}"/>`;
const mirror = (pts) => pts.map(([x, y]) => [1024 - x, y]);

// Envelope silhouette with the flap folded into the top edge, inset by k.
function envelopeContour(k, gapFrom, gapTo) {
  const x0 = 204 + k, x1 = 820 - k, y0 = 304 + k, y1 = 720 - k;
  const apex = 470 + k * 0.35;
  return [[512, apex], [x1, y0], [x1, y1], [x0, y1], [x0, y0]];
}

const options = [
  {
    id: "00-current",
    name: "Current",
    note: "Today's icon, for reference: envelope with a stepped maze flap and two inner walls.",
    glyph: () => [
      `<rect x="204" y="304" width="616" height="416" rx="60"/>`,
      path("M276 376 H356 V456 H436 V536 H588 V456 H668 V376 H748"),
      path("M276 476 V636 H448"),
      path("M576 636 H748 V476"),
    ],
    goal: [512, 436],
  },
  {
    id: "01-stair-flap",
    name: "Stair flap",
    note: "Reduced to the essentials: envelope and a stepped flap that meets at the goal. Reads well at 16 px.",
    glyph: () => [
      `<rect x="204" y="304" width="616" height="416" rx="88"/>`,
      path(rounded([[204, 384], [312, 384], [312, 464], [412, 464], [412, 544], [456, 544]], 34)),
      path(rounded(mirror([[204, 384], [312, 384], [312, 464], [412, 464], [412, 544], [456, 544]]), 34)),
    ],
    goal: [512, 544],
  },
  {
    id: "02-envelope-rings",
    name: "Envelope rings",
    note: "Three nested envelope silhouettes, each with an opening, like a classic labyrinth in the shape of a letter.",
    glyph: () => [
      path(rounded([[204, 304], [512, 500], [820, 304], [820, 720], [204, 720]], 72, true)),
      path(rounded([[440, 624], [300, 624], [300, 470], [512, 604], [724, 470], [724, 624], [584, 624]], 44)),
    ],
    goal: [512, 400],
  },
  {
    id: "03-one-line",
    name: "One line",
    note: "A single continuous stroke: it draws the envelope, folds the flap and winds inward to the goal.",
    glyph: () => [
      path(
        rounded(
          [
            [420, 720], [204, 720], [204, 304], [820, 304], [820, 720], [604, 720],
            [604, 620], [708, 620], [708, 420], [512, 548], [316, 420], [316, 620], [450, 620],
          ],
          64,
        ),
      ),
    ],
    goal: [512, 620],
  },
  {
    id: "04-m-meander",
    name: "M meander",
    note: "The flap is an M drawn as a maze corridor. Works as the M of Maze without the tile, too.",
    glyph: () => [
      `<rect x="204" y="304" width="616" height="416" rx="88"/>`,
      path(rounded([[300, 624], [300, 400], [406, 400], [406, 512], [618, 512], [618, 400], [724, 400], [724, 624]], 40)),
    ],
    goal: [512, 418],
  },
  {
    id: "05-spiral-flap",
    name: "Spiral flap",
    note: "Both flap lines spiral into the center, like a square labyrinth folded into a letter.",
    glyph: () => [
      `<rect x="204" y="304" width="616" height="416" rx="88"/>`,
      path(rounded([[204, 380], [292, 380], [292, 620], [440, 620], [440, 476], [380, 476]], 40)),
      path(rounded(mirror([[204, 380], [292, 380], [292, 620], [440, 620], [440, 476], [380, 476]]), 40)),
      path(rounded([[380, 380], [644, 380]], 0)),
    ],
    goal: [512, 512],
  },
  {
    id: "06-dot-grid",
    name: "Dot grid",
    note: "The envelope and its stepped flap built from a grid of dots, the gaps form the corridor. Matches the particle animation.",
    glyph: () => {
      const cols = 9, rows = 6, x0 = 236, y0 = 326, step = 69;
      const flap = new Set(["1,1", "2,2", "3,3", "5,3", "6,2", "7,1"]);
      const out = [];
      for (let r = 0; r < rows; r++)
        for (let c = 0; c < cols; c++) {
          const edge = r === 0 || r === rows - 1 || c === 0 || c === cols - 1;
          if (edge || flap.has(`${c},${r}`)) {
            const cx = x0 + c * step, cy = y0 + r * step;
            out.push(`<circle cx="${cx}" cy="${cy}" r="24" fill="#FFFFFF" stroke="none"/>`);
          }
        }
      return out;
    },
    goal: [512, 602],
    goalR: 30,
  },
  {
    id: "07-tile-is-envelope",
    name: "Tile as envelope",
    note: "No inner frame: the rounded tile is the envelope, the flap is a stepped maze wall. The boldest at small sizes.",
    glyph: () => [
      path(rounded([[196, 300], [300, 300], [300, 420], [420, 420], [420, 540], [464, 540]], 44)),
      path(rounded(mirror([[196, 300], [300, 300], [300, 420], [420, 420], [420, 540], [464, 540]]), 44)),
      path(rounded([[196, 700], [828, 700]], 0)),
    ],
    goal: [512, 540],
  },
  {
    id: "08-speed",
    name: "Speed",
    note: "Stair flap envelope with three motion lines and a slight forward lean.",
    glyph: () => [
      `<g transform="translate(60 0) skewX(-8) translate(72 0)">`,
      `<rect x="264" y="324" width="540" height="376" rx="84"/>`,
      path(rounded([[264, 396], [356, 396], [356, 468], [446, 468], [446, 540], [490, 540]], 32)),
      path(rounded([[804, 396], [712, 396], [712, 468], [622, 468], [622, 540], [578, 540]], 32)),
      `<circle cx="534" cy="540" r="34" fill="${AMBER}" stroke="none"/>`,
      `</g>`,
      path("M140 420 H232"),
      path("M108 512 H220"),
      path("M140 604 H204"),
    ],
  },
  {
    id: "09-chevrons",
    name: "Nested flaps",
    note: "Diagonal flaps stacked like walls; each one is open on alternating sides, the goal sits in the innermost fold.",
    glyph: () => [
      `<rect x="204" y="304" width="616" height="416" rx="88"/>`,
      path(rounded([[252, 344], [512, 500], [772, 344]], 56)),
      path(rounded([[204, 480], [512, 668], [700, 553]], 56)),
    ],
    goal: [512, 572],
  },
  {
    id: "10-speed-route",
    name: "Route",
    note: "A rounded route enters the envelope, takes two turns and leaves as a send arrow: maze and speed in one gesture.",
    glyph: () => [
      `<rect x="204" y="304" width="616" height="416" rx="88"/>`,
      path(rounded([[112, 400], [400, 400], [400, 624], [624, 624], [624, 512], [912, 512]], 52)),
      path(rounded([[852, 452], [912, 512], [852, 572]], 28)),
    ],
    goal: [512, 512],
    goalOnTop: true,
  },
];

// ---------------------------------------------------------------------------------------
// Round 2: depth. Same palette (graphite, white, amber); depth comes from layering, soft
// shadows, light from above and extrusion, the way Apple's layered icons build it, while
// the flat glyph underneath stays simple enough for 16 px and one-colour use.

const DISLIKED = new Set(["02-envelope-rings", "07-tile-is-envelope", "08-speed", "10-speed-route"]);
const base = (id) => options.find((o) => o.id.startsWith(id));

const defs3d = `
    <linearGradient id="ink" gradientUnits="userSpaceOnUse" x1="0" y1="260" x2="0" y2="760"><stop offset="0" stop-color="#FFFFFF"/><stop offset="1" stop-color="#D4D4D8"/></linearGradient>
    <linearGradient id="plate" gradientUnits="userSpaceOnUse" x1="0" y1="300" x2="0" y2="724"><stop offset="0" stop-color="#F4F4F5"/><stop offset="1" stop-color="#C4C4CA"/></linearGradient>
    <linearGradient id="plateDim" gradientUnits="userSpaceOnUse" x1="0" y1="300" x2="0" y2="724"><stop offset="0" stop-color="#A1A1AA"/><stop offset="1" stop-color="#6E6F78"/></linearGradient>
    <linearGradient id="glass" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#FFFFFF" stop-opacity=".22"/><stop offset="1" stop-color="#FFFFFF" stop-opacity=".04"/></linearGradient>
    <linearGradient id="sheen" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#FFFFFF" stop-opacity=".14"/><stop offset=".5" stop-color="#FFFFFF" stop-opacity="0"/></linearGradient>
    <radialGradient id="amberBall" cx=".36" cy=".32" r=".75"><stop offset="0" stop-color="#FFE3C2"/><stop offset=".45" stop-color="#FFB86B"/><stop offset="1" stop-color="#D98A3E"/></radialGradient>
    <radialGradient id="whiteBall" cx=".36" cy=".32" r=".8"><stop offset="0" stop-color="#FFFFFF"/><stop offset=".6" stop-color="#E4E4E7"/><stop offset="1" stop-color="#9C9CA4"/></radialGradient>
    <filter id="lift" x="-20%" y="-20%" width="140%" height="150%"><feDropShadow dx="0" dy="16" stdDeviation="14" flood-color="#000" flood-opacity=".45"/></filter>
    <filter id="liftSm" x="-20%" y="-20%" width="140%" height="150%"><feDropShadow dx="0" dy="8" stdDeviation="7" flood-color="#000" flood-opacity=".4"/></filter>`;

const strokes = (inner, stroke = "url(#ink)", w = W, extra = "") =>
  `<g fill="none" stroke="${stroke}" stroke-width="${w}" stroke-linecap="round" stroke-linejoin="round"${extra}>${inner}</g>`;
let FLAT_GOAL = false;
const ball = (cx, cy, r = 40) =>
  FLAT_GOAL ? `<circle cx="${cx}" cy="${cy}" r="${r}" fill="${AMBER}"/>` : `<circle cx="${cx}" cy="${cy}" r="${r}" fill="url(#amberBall)" filter="url(#liftSm)"/>`;
// Solid extrusion: n copies stepping toward (dx, dy) in a dark side colour, then the face.
const extrude = (inner, dx, dy, side = "#2B2C33", n = 14, w = W) =>
  Array.from({ length: n }, (_, i) => {
    const k = (n - i) / n;
    return `<g transform="translate(${f(dx * k)} ${f(dy * k)})">${strokes(inner, side, w)}</g>`;
  }).join("");
const sheen = `<rect x="100" y="100" width="824" height="824" rx="185" fill="url(#sheen)"/><rect x="102" y="102" width="820" height="820" rx="183" fill="none" stroke="#FFFFFF" stroke-opacity=".10" stroke-width="4"/>`;

const curGlyph = () => base("00").glyph().join("");
const stairGlyph = () => base("01").glyph().join("");

// Isometric projection of a point on the 1024 canvas, centred on the tile.
const ISO = { s: 0.8, cx: 512, cy: 548 };
const iso = ([x, y], z = 0) => [
  f(ISO.cx + (x - 512) * 0.866 * ISO.s - (y - 512) * 0.866 * ISO.s),
  f(ISO.cy + (x - 512) * 0.5 * ISO.s + (y - 512) * 0.5 * ISO.s - z),
];
const isoT = `matrix(${0.866 * ISO.s} ${0.5 * ISO.s} ${-0.866 * ISO.s} ${0.5 * ISO.s} ${f(ISO.cx - 512 * 0.866 * ISO.s + 512 * 0.866 * ISO.s)} ${f(ISO.cy - 512 * 0.5 * ISO.s - 512 * 0.5 * ISO.s)})`;

// Flap shape whose lower edge is a maze staircase (closed polygon, rounded).
const flapPts = (y0 = 280, dy = 0, dx = 0, rise = 88) => {
  const b = y0 + 100 + dy;
  const half = [[180, y0], [180, b], [324 - dx, b], [324 - dx, b + rise], [412 - dx, b + rise], [412 - dx, b + rise * 2]];
  return [...half, ...mirror(half).reverse()];
};
const envClip = `<clipPath id="env"><rect x="204" y="304" width="616" height="416" rx="80"/></clipPath>`;

const round2 = [
  {
    id: "11-raised-stair",
    name: "Raised stair flap",
    note: "Option 01 lifted off the tile: strokes lit from above with a soft shadow beneath and a glossy goal.",
    flat: base("01"),
    render: () => [sheen, strokes(stairGlyph(), "url(#ink)", W, ` filter="url(#lift)"`), ball(512, 544)],
  },
  {
    id: "12-glass-envelope",
    name: "Glass envelope",
    note: "The current icon with a frosted glass body: the maze sits on a translucent pane that catches light along its top.",
    flat: base("00"),
    render: () => [
      sheen,
      `<rect x="204" y="304" width="616" height="416" rx="60" fill="url(#glass)" filter="url(#lift)"/>`,
      strokes(curGlyph(), "url(#ink)", W, ` filter="url(#liftSm)"`),
      ball(512, 436),
    ],
  },
  {
    id: "13-extruded",
    name: "Extruded",
    note: "The current icon as a solid object: every wall has a short side face, so the maze reads as built, not drawn.",
    flat: base("00"),
    render: () => [sheen, extrude(curGlyph(), 0, 26), strokes(curGlyph()), ball(512, 436)],
  },
  {
    id: "14-iso-maze",
    name: "Isometric maze",
    note: "The envelope laid flat in isometric view with the maze walls standing up out of it, the goal in the middle.",
    flat: base("00"),
    render: () => {
      const walls = [
        "M276 376 H356 V456 H436 V536 H588 V456 H668 V376 H748",
        "M276 476 V636 H448",
        "M576 636 H748 V476",
      ].map((d) => path(d)).join("");
      const H = 56, n = 14;
      const slabTop = `<g transform="${isoT}"><rect x="204" y="304" width="616" height="416" rx="72" fill="#3A3C44"/></g>`;
      const slabSide = Array.from({ length: n }, (_, i) =>
        `<g transform="translate(0 ${f(30 * (1 - i / n))}) ${isoT}"><rect x="204" y="304" width="616" height="416" rx="72" fill="#1C1D22"/></g>`).join("");
      const rim = `<g transform="${isoT}"><rect x="204" y="304" width="616" height="416" rx="72" fill="none" stroke="url(#ink)" stroke-width="44"/></g>`;
      const wallSides = Array.from({ length: n }, (_, i) =>
        `<g transform="translate(0 ${f(-H * (i / n))}) ${isoT}">${strokes(walls, "#8E8F98", 44)}</g>`).join("");
      const wallTops = `<g transform="translate(0 ${-H}) ${isoT}">${strokes(walls, "#FFFFFF", 44)}</g>`;
      const [bx, by] = iso([512, 436], 30);
      return [`<g filter="url(#lift)">${slabSide}${slabTop}</g>`, rim, wallSides, wallTops, `<ellipse cx="${bx}" cy="${by + 6}" rx="34" ry="14" fill="#000" opacity=".35"/>`, ball(bx, by - 20, 34)];
    },
  },
  {
    id: "15-engraved",
    name: "Engraved plate",
    note: "A light metal plate with the stair flap cut into it. Strong silhouette, the depth is in the grooves.",
    flat: base("01"),
    render: () => {
      const flap = base("01").glyph().slice(1).join("");
      return [
        sheen,
        `<rect x="204" y="304" width="616" height="416" rx="88" fill="url(#plate)" filter="url(#lift)"/>`,
        `<rect x="206" y="306" width="612" height="412" rx="86" fill="none" stroke="#FFFFFF" stroke-width="4" opacity=".7"/>`,
        `<defs><clipPath id="plateClip"><rect x="204" y="304" width="616" height="416" rx="88"/></clipPath></defs>`,
        `<g clip-path="url(#plateClip)">${strokes(flap, "#2A2B31", 52)}<g transform="translate(0 7)">${strokes(flap, "#4A4C55", 38)}</g></g>`,
        `<circle cx="512" cy="544" r="40" fill="#2A2B31"/>`,
        `<circle cx="512" cy="548" r="32" fill="url(#amberBall)"/>`,
      ];
    },
  },
  {
    id: "16-one-line-solid",
    name: "One line, solid",
    note: "Option 03 as a thick extruded ribbon: one continuous object you could hold, with a short side face.",
    flat: base("03"),
    render: () => {
      const g = base("03").glyph().join("");
      return [sheen, `<g filter="url(#lift)">${extrude(g, 0, 24)}</g>`, strokes(g), ball(512, 620)];
    },
  },
  {
    id: "17-m-glass",
    name: "M on glass",
    note: "Option 04 with a frosted body and a raised M corridor; the goal floats above the M's middle.",
    flat: base("04"),
    render: () => {
      const g = base("04").glyph();
      return [
        sheen,
        `<rect x="204" y="304" width="616" height="416" rx="88" fill="url(#glass)" filter="url(#lift)"/>`,
        strokes(g.join(""), "url(#ink)", W, ` filter="url(#liftSm)"`),
        ball(512, 418),
      ];
    },
  },
  {
    id: "18-dot-spheres",
    name: "Dot spheres",
    note: "Option 06 with every dot a small lit sphere. The direct starting frame for the particle animation.",
    flat: base("06"),
    render: () => [
      sheen,
      `<g filter="url(#liftSm)">${base("06").glyph().join("").replaceAll('fill="#FFFFFF"', 'fill="url(#whiteBall)"').replaceAll('r="24"', 'r="26"')}</g>`,
      ball(512, 602, 32),
    ],
  },
  {
    id: "19-folded-flap",
    name: "Folded flap",
    note: "Two overlapping layers: the envelope body and a flap whose edge is a maze staircase, with the goal as its seal.",
    flat: base("01"),
    render: () => [
      sheen,
      `<defs>${envClip}</defs>`,
      `<rect x="204" y="304" width="616" height="416" rx="80" fill="url(#plateDim)" filter="url(#lift)"/>`,
      `<g clip-path="url(#env)"><path d="${rounded(flapPts(), 40, true)}" fill="url(#plate)" filter="url(#lift)"/></g>`,
      `<rect x="206" y="306" width="612" height="412" rx="78" fill="none" stroke="#FFFFFF" stroke-width="4" opacity=".5"/>`,
      ball(512, 556, 40),
    ],
  },
  {
    id: "20-terraced-flaps",
    name: "Terraced flaps",
    note: "Option 09's nested flaps as stacked stair-edged layers, like terraces descending into the maze toward the goal.",
    flat: base("09"),
    render: () => [
      sheen,
      `<defs>${envClip}</defs>`,
      `<rect x="204" y="304" width="616" height="416" rx="80" fill="#2E3037" filter="url(#lift)"/>`,
      `<g clip-path="url(#env)">`,
      `<path d="${rounded(flapPts(280, 150, 60), 40, true)}" fill="#6E6F78" filter="url(#lift)"/>`,
      `<path d="${rounded(flapPts(280, 75, 30), 40, true)}" fill="#A1A1AA" filter="url(#lift)"/>`,
      `<path d="${rounded(flapPts(280, 0, 0), 40, true)}" fill="url(#plate)" filter="url(#lift)"/>`,
      `</g>`,
      `<rect x="206" y="306" width="612" height="412" rx="78" fill="none" stroke="#FFFFFF" stroke-width="4" opacity=".35"/>`,
      ball(512, 596, 32),
    ],
  },
];

// ---------------------------------------------------------------------------------------
// Round 3: the M corridor of option 04 (shown on glass in 17) built as a 3D maze in the
// style of 14. Walls are drawn in depth bands from back to front so nearer walls and the
// goal overlap farther ones correctly.

function scene({
  rot = 45, k = 0.58, s = 0.98, cx = 512, cy = 540,
  H = 60, slabH = 30, slab = "solid", rim = "flat", walls = "light", goalZ = 0, bands = 16,
}) {
  const r = (rot * Math.PI) / 180, c = Math.cos(r), sn = Math.sin(r);
  const a = s * c, b = s * k * sn, c2 = -s * sn, d = s * k * c;
  const T = `matrix(${f(a * 1e4) / 1e4} ${f(b * 1e4) / 1e4} ${f(c2 * 1e4) / 1e4} ${f(d * 1e4) / 1e4} ${f(cx - (a + c2) * 512)} ${f(cy - (b + d) * 512)})`;
  const P = ([x, y], z = 0) => [f(a * (x - 512) + c2 * (y - 512) + cx), f(b * (x - 512) + d * (y - 512) + cy - z)];
  const depth = ([x, y]) => sn * (x - 512) + c * (y - 512);

  const rimEl = `<rect x="204" y="304" width="616" height="416" rx="88"/>`;
  const mEl = base("04").glyph()[1];
  const wallEls = (rim === "raised" ? rimEl : "") + mEl;
  const side = walls === "dark" ? "#2B2C33" : "#8E8F98";
  const top = walls === "dark" ? "#6E6F78" : "#FFFFFF";
  const ww = 44, n = 12;

  const slabFill = { solid: "#4A4C55", glass: "#FFFFFF", light: "url(#plate)" }[slab];
  const slabTop = slab === "none" ? "" :
    `<g transform="${T}"><rect x="204" y="304" width="616" height="416" rx="88" fill="${slabFill}"${slab === "glass" ? ` fill-opacity=".16"` : ""}/></g>`;
  const slabSide = slab === "none" ? "" : Array.from({ length: n }, (_, i) =>
    `<g transform="translate(0 ${f(slabH * (1 - i / n))}) ${T}"><rect x="204" y="304" width="616" height="416" rx="88" fill="${slab === "light" ? "#6E6F78" : "#1C1D22"}"${slab === "glass" ? ` fill-opacity=".55"` : ""}/></g>`).join("");
  const slabEdge = slab === "glass"
    ? `<g transform="${T}"><rect x="204" y="304" width="616" height="416" rx="88" fill="none" stroke="#FFFFFF" stroke-opacity=".55" stroke-width="6"/></g>` : "";
  const rimFlat = rim === "flat" ? `<g transform="${T}">${strokes(rimEl, slab === "light" ? "#2B2C33" : "url(#ink)", ww)}</g>` : "";
  const floorShadow = slab === "none"
    ? `<g transform="translate(0 ${slabH + 40}) ${T}" opacity=".45" filter="url(#lift)"><rect x="204" y="304" width="616" height="416" rx="88" fill="#000"/></g>` : "";

  // depth bands
  const corners = [[150, 250], [874, 250], [874, 774], [150, 774]].map(depth);
  const d0 = Math.min(...corners), d1 = Math.max(...corners), step = (d1 - d0) / bands;
  const goal = [512, 418], gd = depth(goal);
  const [gx, gy] = P(goal, goalZ);
  const ballEl = `<ellipse cx="${gx}" cy="${f(P(goal)[1] + 4)}" rx="30" ry="${f(30 * k)}" fill="#000" opacity=".35"/>${ball(gx, f(gy - 30), 32)}`;
  let out = "", ballDone = false;
  for (let i = 0; i < bands; i++) {
    const lo = d0 + i * step, id = `band${rot}${i}${H}${k}`.replace(/\W/g, "");
    if (!ballDone && gd < lo + step / 2) { out += ballEl; ballDone = true; }
    out += `<defs><clipPath id="${id}"><rect x="-2000" y="${f(lo)}" width="4000" height="${f(step + 6)}" transform="translate(512 512) rotate(${-rot})"/></clipPath></defs>`;
    for (let j = 0; j < n; j++)
      out += `<g transform="translate(0 ${f(-H * (j / n))})"><g transform="${T}" clip-path="url(#${id})">${strokes(wallEls, side, ww)}</g></g>`;
    out += `<g transform="translate(0 ${-H})"><g transform="${T}" clip-path="url(#${id})">${strokes(wallEls, top, ww)}</g></g>`;
  }
  if (!ballDone) out += ballEl;
  return [floorShadow, `<g${slab === "none" ? "" : ` filter="url(#lift)"`}>${slabSide}${slabTop}</g>`, slabEdge, rimFlat, out];
}

const round3 = [
  ["21-iso-m", "Isometric M", "17's M corridor in the style of 14: a solid slab, flat envelope rim, the M standing up as walls.", {}],
  ["22-iso-m-glass", "Isometric M on glass", "The slab is a pane of frosted glass with a bright edge, the M walls stand on it.", { slab: "glass" }],
  ["23-iso-m-walled", "Walled envelope", "The envelope outline is a wall too, so the whole letter becomes one maze with the goal inside.", { rim: "raised" }],
  ["24-front-tilt", "Front tilt", "Seen from the front and slightly above instead of from the corner: the envelope reads instantly, the walls still give depth.", { rot: 0, k: 0.55, s: 1.05, cy: 536 }],
  ["25-dimetric-tall", "Dimetric, tall walls", "A flatter 30° angle and taller walls, more architectural.", { rot: 30, k: 0.5, H: 84, cy: 560, s: 0.95, goalZ: 70 }],
  ["26-low-tilt", "Low tilt", "Almost top-down with low walls: the closest to the flat logo, the safest at 16 px.", { rot: 18, k: 0.82, s: 0.92, H: 34, slabH: 20, cy: 522 }],
  ["27-floating", "Floating", "No slab: the walled envelope floats above its own shadow.", { slab: "none", rim: "raised", cy: 520 }],
  ["28-pedestal", "Pedestal", "A thick slab with short walls, like a board game piece.", { slabH: 74, H: 40, cy: 520 }],
  ["29-light-slab", "Light slab", "Inverted: a light plate with graphite walls, lit from above.", { slab: "light", walls: "dark" }],
  ["30-mirror-glass-walled", "Mirrored, glass, walled", "Viewed from the other corner, walled envelope on a glass pane, goal hovering above the floor.", { rot: -45, slab: "glass", rim: "raised", goalZ: 26 }],
].map(([id, name, note, opts]) => ({ id, name, note, flat: base("04"), render: () => scene(opts) }));

// ---------------------------------------------------------------------------------------
// Round 4: variations of 20 (terraced flaps). Each terrace is a plate with real thickness
// resting on the one below, viewed from different angles and in different materials.

const TERRACES = [
  { dy: 150, dx: 60, fill: "#6E6F78", side: "#3A3C44" },
  { dy: 75, dx: 30, fill: "#A1A1AA", side: "#5A5B63" },
  { dy: 0, dx: 0, fill: "url(#plate)", side: "#7C7D86" },
];

function terraces({
  rot = 0, k = 1, s = 1, cx = 512, cy = 512, L = 16, slabH = 0, gap = 0, n = 10,
  layers = TERRACES, body = "#2E3037", bodySide = "#1C1D22", radius = 40, goal = [512, 596],
  outline = false, glass = false, edge = 0.35,
}) {
  const r = (rot * Math.PI) / 180, c = Math.cos(r), sn = Math.sin(r);
  const a = s * c, b = s * k * sn, c2 = -s * sn, d = s * k * c;
  const T = `matrix(${f(a * 1e4) / 1e4} ${f(b * 1e4) / 1e4} ${f(c2 * 1e4) / 1e4} ${f(d * 1e4) / 1e4} ${f(cx - (a + c2) * 512)} ${f(cy - (b + d) * 512)})`;
  const P = ([x, y], z = 0) => [f(a * (x - 512) + c2 * (y - 512) + cx), f(b * (x - 512) + d * (y - 512) + cy - z)];
  const uid = `t${rot}${k}${L}${gap}${radius}${outline ? 1 : 0}${glass ? 1 : 0}`.replace(/\W/g, "");
  const env = `<rect x="204" y="304" width="616" height="416" rx="80"/>`;
  const at = (z, inner, extra = "") => `<g transform="translate(0 ${f(-z)})"><g transform="${T}"${extra}>${inner}</g></g>`;
  const clip = ` clip-path="url(#${uid})"`;
  let out = `<defs><clipPath id="${uid}">${env}</clipPath></defs>`;

  // body slab
  const bodyEl = (fill, extra = "") => env.replace("/>", ` fill="${fill}"${extra}/>`);
  let slab = "";
  for (let i = 0; i < n && slabH; i++) slab += at(-slabH * (1 - i / n), bodyEl(bodySide));
  slab += at(0, outline ? bodyEl("none", ` stroke="url(#ink)" stroke-width="20"`) : bodyEl(body));
  out += `<g filter="url(#lift)">${slab}</g>`;

  // terraces, back (lowest) to front (highest)
  let z = 0;
  layers.forEach((t, li) => {
    const shape = rounded(flapPts(280, t.dy, t.dx), radius, true);
    const base = z + gap;
    const top = base + L;
    let g = "";
    if (outline) {
      const pts = flapPts(280, t.dy, t.dx).slice(1, -1).map(([x, y]) => [Math.min(820, Math.max(204, x)), y]);
      g += at(top, `<path d="${rounded(pts, radius)}" fill="none" stroke="url(#ink)" stroke-width="22" stroke-linecap="round" stroke-linejoin="round"/>`);
    } else {
      const steps = glass ? 1 : n;
      for (let i = 0; i < steps; i++)
        g += at(base + (top - base) * (i / steps), `<path d="${shape}" fill="${t.side}"${glass ? ` fill-opacity=".06"` : ""}/>`, clip);
      g += at(top, `<path d="${shape}" fill="${t.fill}"${glass ? ` fill-opacity="${t.op}"` : ""}/>`, clip);
      g += at(top, `<path d="${shape}" fill="none" stroke="#FFFFFF" stroke-opacity="${edge}" stroke-width="5"/>`, clip);
    }
    out += `<g filter="url(#liftSm)">${g}</g>`;
    z = top;
    if (li === layers.length - 2) {
      const [gx, gy] = P(goal, z);
      out += `<ellipse cx="${gx}" cy="${f(gy + 3)}" rx="28" ry="${f(28 * k)}" fill="#000" opacity=".35"/>`;
      out += `<!--goal:${gx},${f(gy - 28)}-->`;
    }
  });
  // goal last so it never hides behind side faces of the top terrace
  const m = out.match(/<!--goal:([\d.]+),([\d.]+)-->/);
  if (m) out += ball(+m[1], +m[2], 30);
  if (!outline) out += at(0, `<rect x="206" y="306" width="612" height="412" rx="78" fill="none" stroke="#FFFFFF" stroke-opacity=".25" stroke-width="4"/>`);
  return [sheen, out];
}

const FINE = [
  { dy: 165, dx: 66, fill: "#5A5B63", side: "#2E3037" },
  { dy: 110, dx: 44, fill: "#7C7D86", side: "#45464E" },
  { dy: 55, dx: 22, fill: "#A1A1AA", side: "#5A5B63" },
  { dy: 0, dx: 0, fill: "url(#plate)", side: "#7C7D86" },
];
const GLASS = [
  { dy: 150, dx: 60, fill: "#FFFFFF", side: "#FFFFFF", op: ".07" },
  { dy: 75, dx: 30, fill: "#FFFFFF", side: "#FFFFFF", op: ".12" },
  { dy: 0, dx: 0, fill: "#FFFFFF", side: "#FFFFFF", op: ".2" },
];
const INVERTED = [
  { dy: 150, dx: 60, fill: "#71727B", side: "#3A3C44" },
  { dy: 75, dx: 30, fill: "#4A4C55", side: "#2B2C33" },
  { dy: 0, dx: 0, fill: "#2B2C33", side: "#18181B" },
];

const round4 = [
  ["31-stepped", "Stepped", "20 with real thickness: every terrace has a side face, so the steps read as physical.", { L: 18 }],
  ["32-iso-terraces", "Isometric terraces", "The envelope seen from the corner, the terraces rising as solid plates like a stepped maze.", { rot: 45, k: 0.58, s: 0.98, cy: 560, L: 34, slabH: 30 }],
  ["33-front-tilt", "Front tilt", "Seen from the front and above: the flap reads as an envelope first, then as stairs.", { rot: 0, k: 0.62, s: 1.04, cy: 556, L: 30, slabH: 26 }],
  ["34-low-tilt", "Low tilt", "Slightly turned, almost top-down; the closest to the flat 20 and the safest at small sizes.", { rot: 14, k: 0.84, s: 0.96, cy: 530, L: 20, slabH: 18 }],
  ["35-glass", "Glass terraces", "Frosted glass plates with bright edges, each layer a little more opaque than the one behind it.", { layers: GLASS, glass: true, L: 14, edge: 0.6 }],
  ["36-fine-steps", "Four terraces", "Four finer steps with graded shading: deeper, quieter, more like a descending maze.", { layers: FINE, L: 14, goal: [512, 584] }],
  ["37-soft", "Soft steps", "Very round corners, the stairs become soft waves; friendlier and still clearly layered.", { L: 18, radius: 70 }],
  ["38-exploded", "Exploded layers", "The layers lifted apart in isometric view, like Apple's layered icons taken apart.", { rot: 45, k: 0.58, s: 0.94, cy: 600, L: 12, gap: 44, slabH: 16 }],
  ["39-outline", "Outline terraces", "Only the edges, drawn at each height: light, technical, a blueprint of the stairs.", { rot: 45, k: 0.58, s: 0.98, cy: 560, L: 40, outline: true }],
  ["40-inverted", "Inverted", "A light envelope with graphite terraces that get darker toward the front, like steps going down into it.", { layers: INVERTED, body: "url(#plate)", L: 16 }],
].map(([id, name, note, opts]) => ({ id, name, note, flat: base("09"), render: () => terraces(opts) }));

// ---------------------------------------------------------------------------------------
// Round 5: flat versions of 20, drawn as final artwork. Every layer is one exact closed
// path built from the envelope contour and its stair edge: true circular arcs, no clip
// paths, no shapes hanging outside the envelope. checkTerraces() verifies the drawing
// rules below and the build fails if any variant breaks one.

const ENV = { x0: 204, x1: 820, y0: 304, y1: 720, R: 72 };

// Polyline with every interior corner replaced by a circular arc. r is one radius for all
// corners or an array with one radius per interior corner.
const radiusAt = (r, i) => (Array.isArray(r) ? r[i - 1] : r);
function arcRounded(points, r) {
  let d = `M${f(points[0][0])} ${f(points[0][1])}`;
  for (let i = 1; i < points.length - 1; i++) {
    const [ax, ay] = points[i - 1], [bx, by] = points[i], [cx, cy] = points[i + 1];
    const ri = radiusAt(r, i);
    const l1 = Math.hypot(bx - ax, by - ay), l2 = Math.hypot(cx - bx, cy - by);
    const x1 = bx - ((bx - ax) / l1) * ri, y1 = by - ((by - ay) / l1) * ri;
    const x2 = bx + ((cx - bx) / l2) * ri, y2 = by + ((cy - by) / l2) * ri;
    const sweep = (bx - ax) * (cy - by) - (by - ay) * (cx - bx) > 0 ? 1 : 0;
    d += ` L${f(x1)} ${f(y1)} A${ri} ${ri} 0 0 ${sweep} ${f(x2)} ${f(y2)}`;
  }
  const [lx, ly] = points.at(-1);
  return `${d} L${f(lx)} ${f(ly)}`;
}

// The same curve as dense sample points, so checks measure the drawn edge, not the polyline.
function arcSamples(points, r) {
  const out = [points[0]];
  for (let i = 1; i < points.length - 1; i++) {
    const [ax, ay] = points[i - 1], [bx, by] = points[i], [cx, cy] = points[i + 1];
    const ri = radiusAt(r, i);
    const u1 = [(bx - ax) / Math.hypot(bx - ax, by - ay), (by - ay) / Math.hypot(bx - ax, by - ay)];
    const u2 = [(cx - bx) / Math.hypot(cx - bx, cy - by), (cy - by) / Math.hypot(cx - bx, cy - by)];
    const p1 = [bx - u1[0] * ri, by - u1[1] * ri], p2 = [bx + u2[0] * ri, by + u2[1] * ri];
    const centre = [p1[0] + u2[0] * ri, p1[1] + u2[1] * ri];
    const a0 = Math.atan2(p1[1] - centre[1], p1[0] - centre[0]);
    let a1 = Math.atan2(p2[1] - centre[1], p2[0] - centre[0]);
    while (a1 - a0 > Math.PI) a1 -= 2 * Math.PI;
    while (a1 - a0 < -Math.PI) a1 += 2 * Math.PI;
    for (let k = 0; k <= 12; k++) {
      const a = a0 + ((a1 - a0) * k) / 12;
      out.push([centre[0] + ri * Math.cos(a), centre[1] + ri * Math.sin(a)]);
    }
  }
  out.push(points.at(-1));
  return out;
}

// Geometry: n layers, band thickness t (the same horizontally and vertically), first wall
// junction at b0, one or two steps per side. The risers fill the envelope exactly, so the
// band under the back layer is also t thick.
// Corners: r (one radius everywhere), or concentric with rmin: every layer's curves are
// exact offsets of the front layer's, so each band keeps its width through the bends.
// smooth: risers are only arcs (concave + convex radius = riser), the step becomes an S.
function terraceGeom({ n = 3, t = 64, b0 = 392, L1 = 56, w = 72, r = 24, steps = 2, concentric = false, rmin = 20, smooth = false }) {
  const { x0, x1, y1 } = ENV;
  const h = (y1 - b0 - n * t) / steps;
  const s1 = x0 + L1 + (n - 1) * t, s2 = s1 + w;
  if (concentric && smooth) rmin = (h - (n - 1) * t) / 2;
  const layers = Array.from({ length: n }, (_, i) => {
    const b = b0 + i * t, a1 = s1 - i * t, a2 = s2 - i * t;
    const half = steps === 1
      ? [[x0, b], [a1, b], [a1, b + h]]
      : [[x0, b], [a1, b], [a1, b + h], [a2, b + h], [a2, b + 2 * h]];
    const edge = [...half, ...mirror(half).reverse()].map(([x, y]) => [x === 1024 - x0 ? x1 : x, y]);
    // corner kinds along the edge: concave, convex, (concave, convex,) then mirrored
    const kindsHalf = steps === 1 ? ["cc", "cv"] : ["cc", "cv", "cc", "cv"];
    const kinds = [...kindsHalf, ...[...kindsHalf].reverse()];
    // concentric: the front layer has big concave and small convex arcs, each layer
    // further back trades t from the concave radius to the convex one
    const cc = concentric ? rmin + (n - 1 - i) * t : r, cv = concentric ? rmin + i * t : r;
    const radii = kinds.map((k) => (k === "cc" ? cc : cv));
    return { b, a1, a2, tip: b + steps * h, edge, radii };
  });
  const aLast = steps === 1 ? s1 : s2;
  return { n, t, b0, h, r, w, s1, s2, steps, concentric, c: 512 - aLast, layers };
}

function layerPath(L) {
  const { x0, x1, y0, R } = ENV;
  return `${arcRounded(L.edge, L.radii)} L${x1} ${y0 + R} A${R} ${R} 0 0 0 ${x1 - R} ${y0} L${x0 + R} ${y0} A${R} ${R} 0 0 0 ${x0} ${y0 + R} Z`;
}

function goalOf(g, where, r) {
  const front = g.layers[0], back = g.layers.at(-1);
  if (where === "band") return [512, front.tip + g.t / 2];
  if (where === "body") return [512, back.tip + g.t / 2];
  // seal: on the tip of the front flap, or one step up where the tip is too narrow
  if (g.c >= r + 20) return [512, front.tip - r - 18];
  return [512, front.b + g.h - r - 18];
}

const segDist = ([px, py], [ax, ay], [bx, by]) => {
  const dx = bx - ax, dy = by - ay, l = dx * dx + dy * dy;
  const u = l === 0 ? 0 : Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / l));
  return Math.hypot(px - ax - u * dx, py - ay - u * dy);
};

function goalClearance(g, goal, gr) {
  const dmin = Math.min(...g.layers.flatMap((L) => {
    const s = arcSamples(L.edge, L.radii);
    return s.slice(1).map((p, i) => segDist(goal, s[i], p));
  }));
  return Math.min(dmin, ENV.y1 - goal[1], goal[1] - ENV.y0) - gr;
}

// The largest goal circle (in 2 px steps, up to 56) that keeps the 12 px clearance.
function maxGoal(g, where) {
  let best = 16;
  for (let r = 16; r <= 56; r += 2) {
    const c = goalClearance(g, goalOf(g, where, r), r);
    if (!Number.isFinite(c)) throw new Error("goal clearance is not a number");
    if (c >= 12) best = r;
  }
  return best;
}

function checkTerraces(id, g, goal, gr, margin = 12) {
  const { y0, y1, R } = ENV, bad = [];
  if (g.b0 < y0 + R + 8) bad.push(`front layer meets the wall inside the corner arc (y ${g.b0})`);
  if (g.layers.at(-1).b > y1 - R - 8) bad.push(`back layer meets the wall inside the corner arc`);
  if (g.t < 40) bad.push(`band ${g.t} too thin for small sizes`);
  for (const [li, L] of g.layers.entries()) {
    // every segment must hold both of its corner arcs (wall junctions are sharp)
    for (let k = 0; k < L.edge.length - 1; k++) {
      const len = Math.hypot(L.edge[k + 1][0] - L.edge[k][0], L.edge[k + 1][1] - L.edge[k][1]);
      const need = (k > 0 ? L.radii[k - 1] : 0) + (k < L.edge.length - 2 ? L.radii[k] : 0);
      if (len + 0.01 < need) bad.push(`layer ${li} segment ${k}: ${f(len)} px holds less than its arcs (${need} px)`);
    }
    if (Math.min(...L.radii) < 12) bad.push(`layer ${li}: corner radius under 12 px`);
  }
  if (goal) {
    const clear = goalClearance(g, goal, gr);
    if (!Number.isFinite(clear)) bad.push(`goal clearance could not be computed`);
    else if (clear < margin) bad.push(`goal only ${f(clear)} px from an edge`);
  }
  if (bad.length) throw new Error(`${id}: ${bad.join("; ")}`);
  const rs = [...new Set(g.layers.flatMap((L) => L.radii))].sort((a, b) => a - b);
  return `riser ${f(g.h)}, band ${g.t}, corner r ${rs.join("/")}`;
}

const FLAT_TONES = { 2: ["#F4F4F5", "#A1A1AA"], 3: ["#F4F4F5", "#A1A1AA", "#6E6F78"], 4: ["#F4F4F5", "#BDBDC3", "#8E8F98", "#62636C"] };

function flatTerraces({ geom, goal = "band", gr = 26, mode = "fill", tones, body = "#2E3037", tile = null, gap = 22, stroke = 40 }) {
  const g = terraceGeom(geom);
  const G = goalOf(g, goal, gr);
  const { x0, y0, x1, y1, R } = ENV;
  const env = `<rect x="${x0}" y="${y0}" width="${x1 - x0}" height="${y1 - y0}" rx="${R}"`;
  const tn = tones ?? FLAT_TONES[g.n];
  const rMin = Math.min(...g.layers.flatMap((L) => L.radii));
  let art;
  if (mode === "fill") {
    art = `${env} fill="${body}"/>` + g.layers.map((L, i) => `<path d="${layerPath(L)}" fill="${tn[i]}"/>`).reverse().join("");
  } else if (mode === "line") {
    if (rMin - stroke / 2 < 12) throw new Error(`line: inner bend radius ${rMin - stroke / 2} px is nearly sharp`);
    art = strokes(`${env}/>` + g.layers.map((L) => `<path d="${arcRounded(L.edge, L.radii)}"/>`).join(""), "#FFFFFF", stroke);
  } else {
    // cut: one white shape, the stair edges cut out as gaps (true transparency via mask)
    if (rMin - gap / 2 < 8) throw new Error(`cut: inner bend radius ${rMin - gap / 2} px is nearly sharp`);
    const id = `cut${g.n}${g.t}${gap}${g.steps}${rMin}`;
    art = `<defs><mask id="${id}" maskUnits="userSpaceOnUse" x="0" y="0" width="1024" height="1024">${env} fill="#fff"/>` +
      g.layers.map((L) => `<path d="${arcRounded(L.edge, L.radii)}" fill="none" stroke="#000" stroke-width="${gap}"/>`).join("") +
      `</mask></defs><rect x="0" y="0" width="1024" height="1024" fill="#FFFFFF" mask="url(#${id})"/>`;
  }
  return { g, G, art, tile };
}

const round5Specs = [
  ["41-flat-three", "Three terraces", "20 redrawn flat and exact: equal bands, the stair meets the walls on their straight part, the goal centred in the first band.", { geom: { n: 3, t: 72, r: 24 }, goal: "band" }],
  ["42-flat-two", "Two terraces", "One layer less, so the bands can be wider and the goal a full size circle. The calmest version.", { geom: { n: 2, t: 96, r: 30, L1: 64, w: 80 }, goal: "band" }],
  ["43-flat-two-seal", "Two terraces, seal", "Like 42 with the goal on the tip of the front flap, like a wax seal.", { geom: { n: 2, t: 96, r: 30, L1: 64, w: 80 }, goal: "seal" }],
  ["44-flat-three-seal", "Three terraces, seal", "Three layers, the goal sealing the front flap.", { geom: { n: 3, t: 72, r: 24 }, goal: "seal" }],
  ["45-flat-four", "Four terraces", "Four thinner bands, the goal at the bottom of the descent.", { geom: { n: 4, t: 52, r: 20, L1: 44, w: 60 }, goal: "seal" }],
  ["46-flat-line", "Line", "Only the edges: the envelope and its three stair lines in one stroke weight, every bend round on both sides. Works in one colour anywhere.", { geom: { n: 3, t: 72, r: 28 }, goal: "seal", mode: "line", stroke: 30 }],
  ["47-flat-cut", "Cut-out", "One white shape with the stair edges cut out as gaps. Real transparency, so it prints in one colour.", { geom: { n: 3, t: 72, r: 24 }, goal: "band", mode: "cut" }],
  ["48-flat-tile", "Flat tile", "41 on a flat graphite tile instead of the gradient: fully flat, nothing but solid fills.", { geom: { n: 3, t: 72, r: 24 }, goal: "band", tile: "#2A2B31" }],
  ["49-flat-inverted", "Inverted", "Light envelope with graphite terraces darkening toward the front.", { geom: { n: 3, t: 72, r: 24 }, goal: "band", tones: ["#27272A", "#52555F", "#8E8F98"], body: "#E4E4E7" }],
  ["50-flat-soft", "Soft", "Two terraces with the largest corner radius the steps allow: round and friendly.", { geom: { n: 2, t: 96, r: 36, b0: 384, L1: 64, w: 84 }, goal: "band" }],
];


// ---------------------------------------------------------------------------------------
// Round 6: smoother versions of 42 and 50 (two terraces), same rules and checks.
const W2 = { n: 2, b0: 384 };
const S1 = { ...W2, steps: 1, t: 96, L1: 72, concentric: true, smooth: true };
const round6Specs = [
  ["51-wave", "Wave", "50 with the largest possible radius: the arcs meet with no straight riser between them, so each step is a soft S.", { geom: { ...W2, t: 80, r: 44, L1: 64, w: 88 }, goal: "band" }],
  ["52-concentric-steps", "Concentric steps", "42's two steps with concentric corners: the back layer's curves are exact offsets of the front's, so the band stays one width through every bend.", { geom: { ...W2, t: 64, L1: 48, w: 100, concentric: true, rmin: 16 }, goal: "seal" }],
  ["53-concentric-s", "Concentric S", "Two steps, concentric and fully curved: two parallel S-curves per side.", { geom: { ...W2, t: 64, L1: 48, w: 108, concentric: true, smooth: true }, goal: "seal" }],
  ["54-one-step", "One step", "A single step per side with concentric corners: fewer turns, larger curves, calmer at every size.", { geom: { ...W2, steps: 1, t: 88, L1: 72, concentric: true, rmin: 24 }, goal: "band" }],
  ["55-one-wave", "One wave", "A single concentric S per side: the smoothest version, the flap reads as one fluid fold with the goal in its hollow.", { geom: S1, goal: "band" }],
  ["56-one-wave-seal", "One wave, seal", "55 with the goal sealing the tip of the front flap.", { geom: S1, goal: "seal" }],
  ["57-deep-wave", "Deep wave", "Narrower bands and a deeper fold, so the S-curves are larger and slower.", { geom: { ...W2, steps: 1, t: 72, L1: 80, concentric: true, smooth: true }, goal: "seal" }],
  ["58-wave-line", "Wave line", "55 as lines: the envelope and two parallel S-curves in one stroke weight.", { geom: S1, goal: "band", mode: "line", stroke: 24 }],
  ["59-wave-cut", "Wave cut-out", "55 as one white shape with the curves cut out as gaps: real transparency, one colour.", { geom: S1, goal: "band", mode: "cut", gap: 22 }],
  ["60-wave-inverted", "Wave inverted", "55 on a light envelope with graphite layers.", { geom: S1, goal: "band", tones: ["#27272A", "#62636C"], body: "#E4E4E7" }],
];

const makeFlat = ([id, name, note, spec0]) => {
  const g0 = terraceGeom(spec0.geom);
  const spec = { ...spec0, gr: maxGoal(g0, spec0.goal ?? "band") - (spec0.mode === "line" ? Math.ceil((spec0.stroke ?? 40) / 2) : 0) };
  const { g, G, art, tile } = flatTerraces(spec);
  const checked = checkTerraces(id, g, G, spec.gr);
  const goalEl = `<circle cx="${f(G[0])}" cy="${f(G[1])}" r="${spec.gr}" fill="${AMBER}"/>`;
  const mono = (ink) => {
    const rMin = Math.min(...g.layers.flatMap((L) => L.radii));
    const gap = Math.min(spec.gap ?? 22, 2 * (rMin - 8));
    const cut = flatTerraces({ ...spec, gap, mode: spec.mode === "line" ? "line" : "cut" }).art.replaceAll("#FFFFFF", ink);
    return `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">${cut}${goalEl}</svg>`;
  };
  return {
    id, name, note: `${note} <em>Checked: ${checked}.</em>`, mono,
    tileFill: tile, render: () => [art, goalEl],
  };
};
const round5 = round5Specs.map(makeFlat);
const round6 = round6Specs.map(makeFlat);


const defs = `<defs><linearGradient id="bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#52555F"/><stop offset="1" stop-color="#141519"/></linearGradient><clipPath id="tile"><rect x="100" y="100" width="824" height="824" rx="185"/></clipPath></defs>`;

function svg(o, { tile = true, ink = "#FFFFFF" } = {}) {
  if (o.mono && !tile) return o.mono(ink);
  if (o.flat && !tile) return svg(o.flat, { tile, ink });
  if (o.render)
    return `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">\n  <defs><linearGradient id="bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#52555F"/><stop offset="1" stop-color="#141519"/></linearGradient>${defs3d}</defs>\n  <rect x="100" y="100" width="824" height="824" rx="185" fill="${o.tileFill ?? "url(#bg)"}"/>\n  ${o.render().join("\n  ")}\n</svg>\n`;
  const [gx, gy] = o.goal ?? [];
  const body = o.glyph().join("\n    ");
  const g = `<g fill="none" stroke="${ink}" stroke-width="${W}" stroke-linecap="round" stroke-linejoin="round"${o.tileOnly ? ` clip-path="url(#tile)"` : ""}>\n    ${ink === "#FFFFFF" ? body : body.replaceAll("#FFFFFF", ink)}\n  </g>`;
  const goal = o.goal ? dot(gx, gy, o.goalR) : "";
  const frame = tile
    ? `<rect x="100" y="100" width="824" height="824" rx="185" fill="url(#bg)"/>`
    : o.tileOnly
      ? `<rect x="100" y="100" width="824" height="824" rx="185" fill="none" stroke="${ink}" stroke-width="${W}"/>`
      : "";
  return `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">\n  ${defs}\n  ${frame}\n  ${g}\n  ${goal}\n</svg>\n`;
}

let cards = "";
const all = [...options.filter((o) => !DISLIKED.has(o.id)), ...round2, ...round3, ...round4, ...round5, ...round6];
for (const o of all) {
  if (o.id === "11-raised-stair") cards += `\n  <h3>Round 2: depth</h3>`;
  if (o.id === "21-iso-m") cards += `\n  <h3>Round 3: isometric M</h3>`;
  if (o.id === "31-stepped") cards += `\n  <h3>Round 4: terraced flaps</h3>`;
  if (o.id === "51-wave") cards += `\n  <h3>Round 6: smoother 42 and 50</h3>`;
  if (o.id === "41-flat-three") cards += `\n  <h3>Round 5: flat terraces (final artwork, geometry checked)</h3>`;
  const tile = svg(o);
  const light = svg(o, { tile: false, ink: "#18181B" });
  const dark = svg(o, { tile: false, ink: "#F4F4F5" });
  writeFileSync(join(out, `${o.id}.svg`), tile);
  if (o.render) {
    FLAT_GOAL = true;
    writeFileSync(join(out, `${o.id}-circle.svg`), svg(o).replace(/<ellipse[^>]*opacity=".35"\/>/g, ""));
    FLAT_GOAL = false;
  }
  writeFileSync(join(out, `${o.id}-glyph.svg`), light);
  const uri = (s) => `data:image/svg+xml;utf8,${encodeURIComponent(s)}`;
  cards += `
  <article>
    <div class="big"><img src="${o.id}.svg"${o.render ? ` data-circle="${o.id}-circle.svg"` : ""} alt=""></div>
    <div class="row">
      <img src="${uri(tile)}" width="64" alt=""><img src="${uri(tile)}" width="32" alt=""><img src="${uri(tile)}" width="16" alt="">
      <span class="sw light"><img src="${uri(light)}" width="40" alt=""></span>
      <span class="sw dark"><img src="${uri(dark)}" width="40" alt=""></span>
    </div>
    <h2>${o.id.slice(0, 2)} · ${o.name}</h2>
    <p>${o.note}</p>
  </article>`;
}

writeFileSync(
  join(out, "index.html"),
  `<!doctype html><meta charset="utf-8"><title>Maze logo options</title>
<style>
  body{margin:0;padding:32px;background:#0b0b0d;color:#e5e5e8;font:15px/1.5 system-ui,sans-serif}
  h3{grid-column:1/-1;font-weight:500;margin:24px 0 0}
  main{display:grid;grid-template-columns:repeat(auto-fill,minmax(300px,1fr));gap:24px;max-width:1400px;margin:auto}
  article{background:#141417;border:1px solid #242428;border-radius:12px;padding:20px}
  .big img{width:100%;display:block}
  .row{display:flex;align-items:center;gap:14px;margin:12px 0}
  .sw{display:grid;place-items:center;width:56px;height:56px;border-radius:10px}
  .light{background:#f7f7f8}.dark{background:#0b0b0d;border:1px solid #242428}
  h2{font-size:16px;margin:4px 0}p{margin:0;color:#a4a4ac;font-size:14px}
</style>
<h1 style="text-align:center;font-weight:500">Maze logo options</h1>
<p style="text-align:center;margin-bottom:16px"><label><input type="checkbox" id="flat"> Goal as a flat yellow circle (3D options)</label></p>
<script>
  document.addEventListener("change", (e) => {
    if (e.target.id !== "flat") return;
    for (const img of document.querySelectorAll("img[data-circle]")) {
      const other = img.dataset.circle;
      img.dataset.circle = img.getAttribute("src");
      img.setAttribute("src", other);
    }
  });
</script>
<main><h3>Round 1</h3>${cards}
</main>
`,
);
console.log(`wrote ${all.length} options to ${out}`);
