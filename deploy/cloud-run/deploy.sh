#!/usr/bin/env bash
# Deploy the released image to Cloud Run with sources in a private GCS
# bucket in the same region (the shape measured in docs/deploy-targets.md).
#
#   GCP_PROJECT=my-proj BUCKET=my-originals deploy/cloud-run/deploy.sh
#
# REGION (default us-south1), SERVICE (default oximg), IMAGE, CPU,
# MEMORY, CONCURRENCY, MAX_INSTANCES and METRICS=1 are optional. The
# bucket is created if missing (uniform access, public access
# prevention on); the service account gets read-only access to it and
# nothing else, and oximg reads gs:// with the metadata-server token.
set -euo pipefail
: "${GCP_PROJECT:?}" "${BUCKET:?}"
REGION=${REGION:-us-south1}
SERVICE=${SERVICE:-oximg}
IMAGE=${IMAGE:-docker.io/oximg/oximg:0.18.1}

if ! gcloud storage buckets describe "gs://$BUCKET" --project="$GCP_PROJECT" >/dev/null 2>&1; then
  gcloud storage buckets create "gs://$BUCKET" --project="$GCP_PROJECT" \
    --location="$REGION" --uniform-bucket-level-access --public-access-prevention
fi

env_vars="OXIMG_SOURCE_BASE_URL=gs://$BUCKET"
[ "${METRICS:-}" = 1 ] && env_vars="$env_vars,OXIMG_METRICS=1"

gcloud run deploy "$SERVICE" --project="$GCP_PROJECT" --region="$REGION" \
  --image="$IMAGE" --cpu="${CPU:-1}" --memory="${MEMORY:-1Gi}" \
  --concurrency="${CONCURRENCY:-4}" --min-instances=0 \
  --max-instances="${MAX_INSTANCES:-10}" \
  --set-env-vars="$env_vars" --allow-unauthenticated --quiet

sa=$(gcloud run services describe "$SERVICE" --project="$GCP_PROJECT" \
  --region="$REGION" --format='value(spec.template.spec.serviceAccountName)')
gcloud storage buckets add-iam-policy-binding "gs://$BUCKET" \
  --member="serviceAccount:$sa" --role=roles/storage.objectViewer >/dev/null
echo "service account $sa can read gs://$BUCKET"
