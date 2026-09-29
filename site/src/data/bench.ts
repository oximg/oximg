// Benchmark figures shown on the site. Every number here is copied from
// BENCH.md or README.md in this repo — never typed in fresh — and each
// dataset carries enough provenance to find its source table.

const REPO = "https://github.com/oximg/oximg/blob/main";

export type Format = "JPEG" | "PNG" | "WebP" | "AVIF";
export const FORMATS: Format[] = ["JPEG", "PNG", "WebP", "AVIF"];

export interface Cell {
  rps: number;
  p95ms: number;
}

export interface ServerRow {
  server: string;
  ours: boolean;
  cells: Record<Format, Cell>;
}

export interface HarnessRun {
  id: string;
  instance: string;
  arch: string;
  measured: string;
  rows: ServerRow[];
}

export const harness = {
  title: "imgproxy's official benchmark harness",
  workload: "DIV2K corpus over nginx, fit into 512×512, k6 2 VUs × 5 min, all defaults",
  source: `${REPO}/BENCH.md#official-harness-on-real-aws-hardware-c7ilarge-and-c7glarge`,
  runs: [
    {
      id: "c7i",
      instance: "c7i.large",
      arch: "x86-64 · 2 vCPU",
      measured: "2026-07-05",
      rows: [
        {
          server: "oximg",
          ours: true,
          cells: {
            JPEG: { rps: 78.7, p95ms: 33 },
            PNG: { rps: 32.8, p95ms: 79 },
            WebP: { rps: 30.9, p95ms: 92 },
            AVIF: { rps: 15.6, p95ms: 181 },
          },
        },
        {
          server: "imgproxy",
          ours: false,
          cells: {
            JPEG: { rps: 67.0, p95ms: 40 },
            PNG: { rps: 14.3, p95ms: 187 },
            WebP: { rps: 20.3, p95ms: 136 },
            AVIF: { rps: 15.2, p95ms: 190 },
          },
        },
        {
          server: "imagor 1.9.2",
          ours: false,
          cells: {
            JPEG: { rps: 58.7, p95ms: 44 },
            PNG: { rps: 15.5, p95ms: 174 },
            WebP: { rps: 17.7, p95ms: 152 },
            AVIF: { rps: 10.1, p95ms: 283 },
          },
        },
        {
          server: "thumbor 7.x",
          ours: false,
          cells: {
            JPEG: { rps: 50.0, p95ms: 50 },
            PNG: { rps: 8.7, p95ms: 304 },
            WebP: { rps: 14.0, p95ms: 187 },
            AVIF: { rps: 12.1, p95ms: 225 },
          },
        },
      ],
    },
    {
      id: "c7g",
      instance: "c7g.large",
      arch: "Graviton3 · 2 cores",
      measured: "2026-07-05",
      rows: [
        {
          server: "oximg",
          ours: true,
          cells: {
            JPEG: { rps: 91.2, p95ms: 28 },
            PNG: { rps: 39.0, p95ms: 66 },
            WebP: { rps: 41.5, p95ms: 70 },
            AVIF: { rps: 23.4, p95ms: 124 },
          },
        },
        {
          server: "imgproxy",
          ours: false,
          cells: {
            JPEG: { rps: 68.0, p95ms: 39 },
            PNG: { rps: 21.0, p95ms: 123 },
            WebP: { rps: 25.4, p95ms: 110 },
            AVIF: { rps: 20.3, p95ms: 139 },
          },
        },
        {
          server: "imagor 1.9.2",
          ours: false,
          cells: {
            JPEG: { rps: 57.5, p95ms: 44 },
            PNG: { rps: 22.1, p95ms: 115 },
            WebP: { rps: 19.7, p95ms: 133 },
            AVIF: { rps: 13.7, p95ms: 204 },
          },
        },
        {
          server: "thumbor 7.x",
          ours: false,
          cells: {
            JPEG: { rps: 63.2, p95ms: 41 },
            PNG: { rps: 12.5, p95ms: 210 },
            WebP: { rps: 20.2, p95ms: 129 },
            AVIF: { rps: 14.7, p95ms: 196 },
          },
        },
      ],
    },
  ] satisfies HarnessRun[],
};

export interface Proof {
  value: string;
  unit: string;
  label: string;
  versus: string;
  context: string;
  source: string;
}

// The headline strip. Each claim is chosen to be a like-for-like
// comparison: the memory figure is the library one, not the server
// same-URL one, because request coalescing flatters the latter.
export const proofs: Proof[] = [
  {
    value: "91.2",
    unit: "req/s",
    label: "JPEG throughput",
    versus: "imgproxy 68.0",
    context: "c7g.large, official harness",
    source: harness.source,
  },
  {
    value: "2.1",
    unit: "×",
    label: "JPEG → WebP",
    versus: "79.3 vs 37.0 req/s",
    context: "c7g.large, cross-format cell",
    source: `${REPO}/README.md#benchmarks`,
  },
  {
    value: "+6.3",
    unit: "SSIMULACRA2",
    label: "sharper at the same q80",
    versus: "77.5 vs imgproxy 71.2",
    context: "Kodak corpus, end-to-end JPEG",
    source: `${REPO}/bench/quality/QUALITY.md`,
  },
  {
    value: "37.5",
    unit: "MB",
    label: "peak RSS as a library",
    versus: "ruby-vips 373.3 MB",
    context: "4000×2667 → 750×500, M2 Max",
    source: `${REPO}/BENCH.md#ruby-as-a-library-against-the-image-processing-gems`,
  },
];

export interface QualityPair {
  label: string;
  detail: string;
  ours: number;
  theirs: number;
  theirsName: string;
}

export const quality: QualityPair[] = [
  {
    label: "End-to-end JPEG, q80",
    detail: "Kodak corpus; the gap widens with source size",
    ours: 77.5,
    theirs: 71.2,
    theirsName: "imgproxy",
  },
  {
    label: "Pure resize (lossless PNG path)",
    detail: "isolates the resampler from the encoder",
    ours: 97.6,
    theirs: 81.9,
    theirsName: "imgproxy",
  },
];

// --- /benchmarks ---------------------------------------------------------

const BENCH = `${REPO}/BENCH.md`;

// Which build the throughput tables were measured on. BENCH.md's Notes:
// every throughput table predates the 2026-08 decode-scale change
// (shipped in 0.11.0), after which JPEG sources decode at full size.
export const measuredBefore = {
  version: "0.11.0",
  change: "made full-size JPEG decode the default",
  example: "a 2000px source re-measured 17% slower locally",
  source: `${BENCH}#notes`,
};

export const nextGen: HarnessRun[] = [
  {
    id: "c8i",
    instance: "c8i.large",
    arch: "Granite Rapids · 2 vCPU",
    measured: "2026-07",
    rows: [
      {
        server: "oximg",
        ours: true,
        cells: {
          JPEG: { rps: 110.3, p95ms: 24 },
          PNG: { rps: 44.7, p95ms: 58 },
          WebP: { rps: 40.7, p95ms: 70 },
          AVIF: { rps: 21.5, p95ms: 134 },
        },
      },
      {
        server: "imgproxy",
        ours: false,
        cells: {
          JPEG: { rps: 90.3, p95ms: 31 },
          PNG: { rps: 19.0, p95ms: 142 },
          WebP: { rps: 27.0, p95ms: 104 },
          AVIF: { rps: 20.8, p95ms: 140 },
        },
      },
      {
        server: "imagor 1.9.2",
        ours: false,
        cells: {
          JPEG: { rps: 76.9, p95ms: 34 },
          PNG: { rps: 20.8, p95ms: 130 },
          WebP: { rps: 24.5, p95ms: 110 },
          AVIF: { rps: 14.5, p95ms: 199 },
        },
      },
      {
        server: "thumbor 7.x",
        ours: false,
        cells: {
          JPEG: { rps: 66.4, p95ms: 38 },
          PNG: { rps: 11.2, p95ms: 235 },
          WebP: { rps: 18.6, p95ms: 139 },
          AVIF: { rps: 16.0, p95ms: 171 },
        },
      },
    ],
  },
  {
    id: "c9g",
    instance: "c9g.large",
    arch: "next-gen Graviton · 2 cores",
    measured: "2026-07",
    rows: [
      {
        server: "oximg",
        ours: true,
        cells: {
          JPEG: { rps: 135.6, p95ms: 19 },
          PNG: { rps: 53.9, p95ms: 48 },
          WebP: { rps: 58.9, p95ms: 51 },
          AVIF: { rps: 36.2, p95ms: 82 },
        },
      },
      {
        server: "imgproxy",
        ours: false,
        cells: {
          JPEG: { rps: 112.8, p95ms: 25 },
          PNG: { rps: 32.8, p95ms: 80 },
          WebP: { rps: 36.3, p95ms: 79 },
          AVIF: { rps: 32.4, p95ms: 90 },
        },
      },
      {
        server: "imagor 1.9.2",
        ours: false,
        cells: {
          JPEG: { rps: 100.6, p95ms: 26 },
          PNG: { rps: 34.5, p95ms: 75 },
          WebP: { rps: 29.7, p95ms: 88 },
          AVIF: { rps: 22.3, p95ms: 129 },
        },
      },
      {
        server: "thumbor 7.x",
        ours: false,
        cells: {
          JPEG: { rps: 100.4, p95ms: 26 },
          PNG: { rps: 18.7, p95ms: 140 },
          WebP: { rps: 30.6, p95ms: 86 },
          AVIF: { rps: 22.1, p95ms: 135 },
        },
      },
    ],
  },
];

export const nextGenSource = `${BENCH}#newer-instance-generations-c8ilarge-and-c9glarge`;

export interface CrossCell {
  instance: string;
  webp: { ours: Cell; theirs: Cell };
  avif: { ours: Cell; theirs: Cell };
}

// JPEG sources, converted; oximg vs imgproxy only (our harness extension).
export const crossFormat = {
  source: `${BENCH}#cross-format-cells-our-extension-of-the-harness`,
  cells: [
    {
      instance: "c7i.large",
      webp: { ours: { rps: 65.3, p95ms: 41 }, theirs: { rps: 35.3, p95ms: 73 } },
      avif: { ours: { rps: 44.6, p95ms: 57 }, theirs: { rps: 44.9, p95ms: 59 } },
    },
    {
      instance: "c7g.large",
      webp: { ours: { rps: 79.3, p95ms: 33 }, theirs: { rps: 37.0, p95ms: 69 } },
      avif: { ours: { rps: 56.5, p95ms: 46 }, theirs: { rps: 52.7, p95ms: 50 } },
    },
    {
      instance: "c8i.large",
      webp: { ours: { rps: 89.6, p95ms: 30 }, theirs: { rps: 46.7, p95ms: 55 } },
      avif: { ours: { rps: 64.8, p95ms: 40 }, theirs: { rps: 63.4, p95ms: 43 } },
    },
    {
      instance: "c9g.large",
      webp: { ours: { rps: 116.6, p95ms: 23 }, theirs: { rps: 56.8, p95ms: 46 } },
      avif: { ours: { rps: 96.9, p95ms: 27 }, theirs: { rps: 87.6, p95ms: 33 } },
    },
  ] satisfies CrossCell[],
  // c7i.large JPEG→AVIF, interleaved official cells.
  avifSpeed9: {
    tuned: { rps: 53.3, p95ms: 48 },
    imgproxy: { rps: 45.8, p95ms: 58 },
    default: { rps: 44.8, p95ms: 57 },
  },
};

export interface FrontierPoint {
  label: string;
  margin: string;
  rps: number;
  ssim2: number;
  ours: boolean;
  isDefault?: boolean;
  approx?: boolean;
}

// The current default, shown as a frontier rather than one cell.
export const frontier = {
  workload: "7360×4912 → 500×500, ab -n 300 -c 8, three interleaved rounds",
  machine: "Apple M2 Max",
  measured: "2026-08",
  source: `${BENCH}#decode-scale-the-throughputquality-frontier-2026-08`,
  points: [
    { label: "full size", margin: "unset", rps: 41.3, ssim2: 76.9, ours: true, isDefault: true },
    { label: "DCT ≤ 2×", margin: "~7", rps: 48.4, ssim2: 75.7, ours: true },
    { label: "DCT ≤ 4×", margin: "~3.5", rps: 62.7, ssim2: 71.5, ours: true },
    { label: "DCT does the lot", margin: "1.7", rps: 71.4, ssim2: 59.4, ours: true },
    { label: "imgproxy 4.0.9", margin: "—", rps: 60.7, ssim2: 56, ours: false, approx: true },
  ] satisfies FrontierPoint[],
};

export interface CapacityRow {
  c: number;
  ours: { rps?: number; p50: string; tail: string; fail: string; mem: number };
  theirs: { rps?: number; p50: string; tail: string; fail: string; mem: number };
  coalesced?: boolean;
}

// Constant open connections, each on its own URL; 4 cores + SMT.
export const capacity = {
  workload: "k6 VUs on distinct URLs (4100-URL space), 30 s per level, DIV2K 512-fit JPEG",
  machine: "Ryzen 7 8745HS, server on 4 cores + SMT",
  source: `${BENCH}#connection-capacity-and-overload-behavior`,
  rows: [
    {
      c: 16,
      ours: { rps: 649, p50: "24 ms", tail: "p99 31 ms", fail: "0%", mem: 50 },
      theirs: { rps: 444, p50: "36 ms", tail: "p99 48 ms", fail: "0%", mem: 128 },
    },
    {
      c: 256,
      ours: { rps: 598, p50: "0.43 s", tail: "p99 0.44 s", fail: "0%", mem: 62 },
      theirs: { rps: 443, p50: "0.58 s", tail: "p99 0.64 s", fail: "0%", mem: 167 },
    },
    {
      c: 1024,
      ours: { rps: 587, p50: "1.7 s", tail: "p99 1.8 s", fail: "0%", mem: 88 },
      theirs: { rps: 439, p50: "2.3 s", tail: "p99 2.4 s", fail: "0%", mem: 248 },
    },
    {
      c: 2048,
      ours: { rps: 578, p50: "3.5 s", tail: "p99 4.2 s", fail: "0%", mem: 106 },
      theirs: { rps: 439, p50: "4.6 s", tail: "p99 4.7 s", fail: "0%", mem: 358 },
    },
    {
      c: 4096,
      ours: { rps: 565, p50: "7.2 s", tail: "p99 8.9 s", fail: "0%", mem: 168 },
      theirs: { rps: 489, p50: "4.8 s", tail: "p95 30 s", fail: "12%", mem: 361 },
    },
    {
      c: 8192,
      coalesced: true,
      ours: { p50: "—", tail: "—", fail: "0%", mem: 265 },
      theirs: { p50: "—", tail: "—", fail: "29%", mem: 356 },
    },
  ] satisfies CapacityRow[],
};

export const coldStart = {
  machine: "Ryzen 7 8745HS, local Docker (runc)",
  source: `${BENCH}#cold-start`,
  rows: [
    { what: "oximg native binary", ready: "6 / 6 ms", firstWork: "13 / 14 ms", ours: true },
    { what: "oximg Docker", ready: "124 / 192 ms", firstWork: "132 / 199 ms", ours: true },
    { what: "imgproxy Docker", ready: "138 / 143 ms", firstWork: "145 / 151 ms", ours: false },
  ],
  footprint: [
    { what: "container image", ours: "113 MB", theirs: "235 MB" },
    { what: "idle RSS after ready", ours: "10 MB", theirs: "29 MB" },
  ],
};

// Same harness over DIV2K variants with EXIF orientation or an sRGB ICC
// profile spliced in; req/s as the mean of two interleaved rounds.
export const metadata = {
  machine: "Ryzen 7 8745HS, cpuset 0,1, 2 VUs, oximg 0.4.4",
  source: `${BENCH}#metadata-sources-orientation--icc`,
  rows: [
    { cell: "clean JPEG→JPEG", ours: [194.0, 200.5], theirs: [163.3, 162.7], lead: 21 },
    { cell: "oriented JPEG→JPEG", ours: [190.1, 197.5], theirs: [156.4, 157.8], lead: 23 },
    { cell: "ICC-profiled JPEG→JPEG", ours: [195.3, 199.7], theirs: [130.8, 130.6], lead: 51 },
    { cell: "oriented JPEG→AVIF", ours: [115.1, 115.8], theirs: [101.6, 100.6], lead: 14 },
    { cell: "ICC-profiled JPEG→AVIF", ours: [119.3, 117.4], theirs: [92.1, 91.6], lead: 29 },
  ],
};

// The Ruby gem against what an ActiveStorage app would otherwise call.
export const ruby = {
  workload: "4000×2667 JPEG → fit 750×750, q80, each gem at its defaults, best of 3",
  machine: "Apple M2 Max",
  versions: "oximg 0.10.1, ruby-vips 2.3.0, image_processing 1.14.0, mini_magick 5.3.3",
  source: `${BENCH}#ruby-as-a-library-against-the-image-processing-gems`,
  rows: [
    { gem: "oximg", ms: 73.4, cpu: 0.83, rss: 37.5, ours: true },
    { gem: "ruby-vips", ms: 74.0, cpu: 0.91, rss: 373.3, ours: false },
    { gem: "image_processing/vips", ms: 73.1, cpu: 0.95, rss: 156.5, ours: false },
    { gem: "image_processing/magick", ms: 317.9, cpu: 2.97, rss: 181.2, ours: false },
    { gem: "mini_magick", ms: 316.3, cpu: 2.97, rss: 182.2, ours: false },
  ],
};
