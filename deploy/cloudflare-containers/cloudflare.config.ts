import { bindings, defineConfig, defineContainer, defineWorker, exports } from "cf/config";

// Deploy-time settings, read from the shell running `cf deploy`. The
// two OXIMG_* ones are server knobs passed through to the container.
//   OXIMG_SOURCE_BASE_URL  required: https://… origin (see docs/deploy-cloudflare.md)
//   OXIMG_METRICS          "1" exposes /metrics (bench/platforms needs it)
//   INSTANCES              container instances to spread URLs over (default 1)
//   INSTANCE_TYPE          lite | basic | standard-1..4 (default standard-2, the measured one)
//   IMAGE                  default docker.io/oximg/oximg:0.18.1
const source = process.env.OXIMG_SOURCE_BASE_URL;
if (!source) throw new Error("set OXIMG_SOURCE_BASE_URL before cf deploy");
const instances = process.env.INSTANCES ?? "1";

// The released native image, unchanged.
const oximg = defineContainer({
	name: "oximg",
	image: { reference: process.env.IMAGE ?? "docker.io/oximg/oximg:0.18.1" },
	instanceType: (process.env.INSTANCE_TYPE ?? "standard-2") as "standard-2",
	maxInstances: Number(instances),
});

const worker = defineWorker({
	name: "oximg-containers",
	compatibilityDate: "2026-06-01",
	entrypoint: "src/index.js",
	observability: { enabled: true },
	env: {
		OXIMG: bindings.durableObject({ worker: "oximg-containers", exportName: "OximgContainer" }),
		OXIMG_SOURCE_BASE_URL: bindings.text(source),
		INSTANCES: bindings.text(instances),
		OXIMG_METRICS: bindings.text(process.env.OXIMG_METRICS ?? ""),
	},
	exports: {
		OximgContainer: exports.durableObject({ storage: "sqlite", container: oximg }),
	},
});

export default defineConfig({ worker, containers: [oximg] });
