// k6 connection-capacity script: N concurrent connections (VUs), each
// pinned to its own distinct URL out of a 4100-URL space (100 files x
// 41 fit-widths), so request coalescing can never serve duplicates and
// the measurement is true processing capacity per connection.
//
// Env: BASE (http://host:port), KIND (oximg|imgproxy), VUS, DURATION.
import http from 'k6/http';
import exec from 'k6/execution';

const VUS = Number(__ENV.VUS || 64);
const BASE = __ENV.BASE;
const KIND = __ENV.KIND || 'oximg';

export const options = {
  scenarios: {
    load: {
      executor: 'constant-vus',
      vus: VUS,
      duration: __ENV.DURATION || '30s',
      gracefulStop: '30s',
    },
  },
  discardResponseBodies: true,
  summaryTrendStats: ['avg', 'p(50)', 'p(95)', 'p(99)', 'max'],
};

const FILES = 100; // DIV2K 0801.jpg .. 0900.jpg
const WIDTHS = 41; // 472..512

function urlFor(i) {
  const file = 801 + (i % FILES);
  const w = 472 + (Math.floor(i / FILES) % WIDTHS);
  return KIND === 'imgproxy'
    ? `${BASE}/insecure/rs:fit:${w}:512/plain/local:///0${file}.jpg`
    : `${BASE}/resize/${w}/512/0${file}.jpg`;
}

export default function () {
  const i = (exec.vu.idInTest - 1) % (FILES * WIDTHS);
  http.get(urlFor(i), { timeout: '30s' });
}
