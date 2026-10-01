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
      measured: "2026-09-29",
      rows: [
        {
          server: "oximg",
          ours: true,
          cells: {
            JPEG: { rps: 62.8, p95ms: 41 },
            PNG: { rps: 37.3, p95ms: 69 },
            WebP: { rps: 35.6, p95ms: 80 },
            AVIF: { rps: 17.8, p95ms: 159 },
          },
        },
        {
          server: "imgproxy",
          ours: false,
          cells: {
            JPEG: { rps: 74.4, p95ms: 37 },
            PNG: { rps: 16.6, p95ms: 162 },
            WebP: { rps: 23.1, p95ms: 120 },
            AVIF: { rps: 17.4, p95ms: 167 },
          },
        },
        {
          server: "imagor 1.9.6",
          ours: false,
          cells: {
            JPEG: { rps: 67.0, p95ms: 39 },
            PNG: { rps: 17.6, p95ms: 153 },
            WebP: { rps: 19.0, p95ms: 140 },
            AVIF: { rps: 11.4, p95ms: 252 },
          },
        },
        {
          server: "thumbor 7.8.0",
          ours: false,
          cells: {
            JPEG: { rps: 55.5, p95ms: 45 },
            PNG: { rps: 9.8, p95ms: 271 },
            WebP: { rps: 15.6, p95ms: 166 },
            AVIF: { rps: 13.1, p95ms: 208 },
          },
        },
      ],
    },
    {
      id: "c7g",
      instance: "c7g.large",
      arch: "Graviton3 · 2 cores",
      measured: "2026-09-29",
      rows: [
        {
          server: "oximg",
          ours: true,
          cells: {
            JPEG: { rps: 64.7, p95ms: 38 },
            PNG: { rps: 39.9, p95ms: 65 },
            WebP: { rps: 41.4, p95ms: 69 },
            AVIF: { rps: 24.5, p95ms: 120 },
          },
        },
        {
          server: "imgproxy",
          ours: false,
          cells: {
            JPEG: { rps: 67.3, p95ms: 40 },
            PNG: { rps: 21.3, p95ms: 123 },
            WebP: { rps: 25.6, p95ms: 110 },
            AVIF: { rps: 20.3, p95ms: 140 },
          },
        },
        {
          server: "imagor 1.9.6",
          ours: false,
          cells: {
            JPEG: { rps: 57.5, p95ms: 44 },
            PNG: { rps: 22.2, p95ms: 115 },
            WebP: { rps: 19.5, p95ms: 132 },
            AVIF: { rps: 13.6, p95ms: 208 },
          },
        },
        {
          server: "thumbor 7.8.0",
          ours: false,
          cells: {
            JPEG: { rps: 60.6, p95ms: 42 },
            PNG: { rps: 12.3, p95ms: 213 },
            WebP: { rps: 20.1, p95ms: 130 },
            AVIF: { rps: 14.7, p95ms: 198 },
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
    value: "39.9",
    unit: "req/s",
    label: "PNG throughput",
    versus: "imgproxy 21.3",
    context: "c7g.large, official harness",
    source: harness.source,
  },
  {
    value: "1.6",
    unit: "×",
    label: "JPEG → WebP",
    versus: "58.2 vs 37.2 req/s",
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
// the AWS harness grids were re-measured on 0.12.0; every other
// throughput table predates the 2026-08 decode-scale change (shipped in
// 0.11.0), after which JPEG sources decode at full size.
export const awsMeasuredOn = { version: "0.12.0", date: "2026-09-29" };

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
    measured: "2026-09-29",
    rows: [
      {
        server: "oximg",
        ours: true,
        cells: {
          JPEG: { rps: 73.7, p95ms: 34 },
          PNG: { rps: 43.3, p95ms: 59 },
          WebP: { rps: 40.7, p95ms: 71 },
          AVIF: { rps: 20.7, p95ms: 139 },
        },
      },
      {
        server: "imgproxy",
        ours: false,
        cells: {
          JPEG: { rps: 88.2, p95ms: 31 },
          PNG: { rps: 18.6, p95ms: 145 },
          WebP: { rps: 26.6, p95ms: 105 },
          AVIF: { rps: 20.0, p95ms: 146 },
        },
      },
      {
        server: "imagor 1.9.6",
        ours: false,
        cells: {
          JPEG: { rps: 75.5, p95ms: 34 },
          PNG: { rps: 20.0, p95ms: 135 },
          WebP: { rps: 23.2, p95ms: 115 },
          AVIF: { rps: 13.9, p95ms: 206 },
        },
      },
      {
        server: "thumbor 7.8.0",
        ours: false,
        cells: {
          JPEG: { rps: 65.5, p95ms: 38 },
          PNG: { rps: 11.0, p95ms: 239 },
          WebP: { rps: 17.6, p95ms: 145 },
          AVIF: { rps: 15.1, p95ms: 181 },
        },
      },
    ],
  },
  {
    id: "c9g",
    instance: "c9g.large",
    arch: "next-gen Graviton · 2 cores",
    measured: "2026-09-29",
    rows: [
      {
        server: "oximg",
        ours: true,
        cells: {
          JPEG: { rps: 98.0, p95ms: 26 },
          PNG: { rps: 55.4, p95ms: 46 },
          WebP: { rps: 59.2, p95ms: 50 },
          AVIF: { rps: 37.4, p95ms: 80 },
        },
      },
      {
        server: "imgproxy",
        ours: false,
        cells: {
          JPEG: { rps: 112.6, p95ms: 25 },
          PNG: { rps: 33.1, p95ms: 78 },
          WebP: { rps: 36.9, p95ms: 78 },
          AVIF: { rps: 32.4, p95ms: 90 },
        },
      },
      {
        server: "imagor 1.9.6",
        ours: false,
        cells: {
          JPEG: { rps: 100.2, p95ms: 26 },
          PNG: { rps: 34.9, p95ms: 74 },
          WebP: { rps: 30.3, p95ms: 87 },
          AVIF: { rps: 22.5, p95ms: 129 },
        },
      },
      {
        server: "thumbor 7.8.0",
        ours: false,
        cells: {
          JPEG: { rps: 98.1, p95ms: 26 },
          PNG: { rps: 18.5, p95ms: 141 },
          WebP: { rps: 30.5, p95ms: 87 },
          AVIF: { rps: 22.0, p95ms: 133 },
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
      webp: { ours: { rps: 55.4, p95ms: 47 }, theirs: { rps: 38.1, p95ms: 68 } },
      avif: { ours: { rps: 40.3, p95ms: 63 }, theirs: { rps: 47.4, p95ms: 56 } },
    },
    {
      instance: "c7g.large",
      webp: { ours: { rps: 58.2, p95ms: 44 }, theirs: { rps: 37.2, p95ms: 69 } },
      avif: { ours: { rps: 47.7, p95ms: 53 }, theirs: { rps: 52.2, p95ms: 51 } },
    },
    {
      instance: "c8i.large",
      webp: { ours: { rps: 64.3, p95ms: 40 }, theirs: { rps: 46.6, p95ms: 56 } },
      avif: { ours: { rps: 50.4, p95ms: 51 }, theirs: { rps: 62.0, p95ms: 44 } },
    },
    {
      instance: "c9g.large",
      webp: { ours: { rps: 86.0, p95ms: 30 }, theirs: { rps: 57.3, p95ms: 45 } },
      avif: { ours: { rps: 76.6, p95ms: 33 }, theirs: { rps: 88.5, p95ms: 33 } },
    },
  ] satisfies CrossCell[],
};

export interface ControlCell {
  instance: string;
  /** Pre-0.11.0 decode default, `OXIMG_DCT_MARGIN=1.7`. */
  margin17: Cell;
  /** imgproxy, the control round's same-run anchor. */
  imgproxy: Cell;
}

// The harness JPEG cell at the old shrink-on-load default, measured after
// each instance's grid in the same run.
export const decodeControl = {
  source: `${BENCH}#decode-default-control-cells`,
  cells: [
    { instance: "c7i.large", margin17: { rps: 89.3, p95ms: 30 }, imgproxy: { rps: 76.2, p95ms: 36 } },
    { instance: "c7g.large", margin17: { rps: 97.2, p95ms: 27 }, imgproxy: { rps: 67.6, p95ms: 39 } },
    { instance: "c8i.large", margin17: { rps: 109.5, p95ms: 24 }, imgproxy: { rps: 87.8, p95ms: 31 } },
    { instance: "c9g.large", margin17: { rps: 146.8, p95ms: 18 }, imgproxy: { rps: 113.6, p95ms: 25 } },
  ] satisfies ControlCell[],
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
