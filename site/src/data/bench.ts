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
  // Since 0.11.0 JPEG sources decode at full size, so these JPEG cells
  // overstate current releases (BENCH.md, "JPEG sources since 0.11").
  caveat:
    "JPEG cells predate 0.11.0's full-size decode; on a 2026-10 Zen 4 re-run, 0.13.0 leads imgproxy 4.0.17 by 7–13% on JPEG",
  caveatSource: `${REPO}/BENCH.md#jpeg-sources-since-011-2026-10`,
  runs: [
    {
      id: "c7i",
      instance: "c7i.large",
      arch: "x86-64 · 2 vCPU",
      measured: "2026-07-05 on a pre-0.11 build",
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
      measured: "2026-07-05 on a pre-0.11 build",
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
    context: "c7g.large, official harness, pre-0.11 build",
    source: harness.source,
  },
  {
    value: "2.1",
    unit: "×",
    label: "JPEG → WebP",
    versus: "79.3 vs 37.0 req/s",
    context: "c7g.large, cross-format cell, pre-0.11 build",
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
