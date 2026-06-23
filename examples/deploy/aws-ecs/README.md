# PowDB on AWS ECS Fargate + EFS

A minimal Terraform module that runs `ghcr.io/zvn-dev/powdb:v0.6.1` as a
single Fargate task with persistent storage on EFS. This is a starting
point, not a turnkey production deploy — read the trade-offs below before
you `terraform apply`.

## What it provisions

- ECS cluster + Fargate service (`desired_count = 1`).
- EFS file system mounted at `/data` inside the container, backing
  `POWDB_DATA`. Encrypted at rest, encrypted in transit.
- Security groups: PowDB task SG (TCP `5433` ingress from
  `allowed_cidr_blocks`) and EFS SG (NFS `2049` ingress from the task SG
  only).
- IAM execution role + a policy fragment that lets the task read your
  `POWDB_PASSWORD` from Secrets Manager.
- CloudWatch log group `/ecs/powdb` with 14-day retention.

It does **not** provision:

- A load balancer / TLS terminator. PowDB speaks raw TCP, so either enable
  PowDB's own TLS (mount the cert/key in via EFS, point `POWDB_TLS_CERT`
  and `POWDB_TLS_KEY` at them) or front the service with an NLB + your
  own ACM cert if you need TLS termination at the edge.
- A dedicated VPC. The module uses the account's default VPC so the
  example stays small. For production, swap `data.aws_vpc.default` for
  your own VPC + private subnets + a NAT path.
- Backups. Add an `aws_backup_plan` against the EFS file system, or
  schedule snapshots via AWS Backup.

## Trade-offs you're accepting

- **Single writer.** PowDB does not support multi-writer; never scale this
  service past `desired_count = 1`. Two tasks sharing the EFS dir will
  corrupt the WAL.
- **EFS fsync latency.** Higher than local NVMe. Acceptable for the
  embedded/hobby workloads this example targets. If you need the WAL on
  faster storage, switch to EBS via a Fargate-incompatible launch type
  (EC2 + an EBS volume) or use the Fly.io example instead.
- **No service discovery.** Each Fargate task run gets a fresh ENI. The
  `powdb_endpoint_hint` output documents the workaround; for production,
  put an NLB in front and target the ECS service.

## Quickstart

```bash
# 1. Create the password secret out-of-band:
aws secretsmanager create-secret \
  --name powdb-prod-password \
  --secret-string "$(openssl rand -hex 32)"

# Capture the ARN it prints — you'll pass it as a variable.

# 2. Plan + apply:
cd examples/deploy/aws-ecs
terraform init
terraform plan \
  -var "powdb_password_secret_arn=arn:aws:secretsmanager:us-east-1:123456789012:secret:powdb-prod-password-AbCdEf"
terraform apply \
  -var "powdb_password_secret_arn=arn:aws:secretsmanager:us-east-1:123456789012:secret:powdb-prod-password-AbCdEf"
```

`terraform apply` typically takes ~3 minutes (ECS service + EFS mount
targets dominate). The cluster, service, and EFS ID are printed as
outputs; resolve the running task's ENI to find the IP, or put an NLB in
front for a stable DNS name.

## Variables (high-signal subset)

| Name | Default | Notes |
|---|---|---|
| `powdb_password_secret_arn` | _(required)_ | Secrets Manager ARN holding `POWDB_PASSWORD`. |
| `powdb_image` | `ghcr.io/zvn-dev/powdb:v0.6.1` | Pin to a digest in production. |
| `powdb_port` | `5433` | TCP wire protocol port. |
| `task_cpu` | `512` (0.5 vCPU) | Fargate CPU units. |
| `task_memory` | `1024` MiB | Must satisfy Fargate cpu↔memory ratios. |
| `require_tls` | `true` | Sets `POWDB_REQUIRE_TLS=1` so the server refuses to start with a password but no TLS. |
| `query_memory_limit_bytes` | `268435456` (256 MiB) | `POWDB_QUERY_MEMORY_LIMIT`. |
| `allowed_cidr_blocks` | RFC1918 ranges | Override for public access. |
| `assign_public_ip` | `false` | Set `true` if running in a public subnet without a NAT. |

See [`variables.tf`](./variables.tf) for the full list.

## Validation (no AWS account needed)

```bash
cd examples/deploy/aws-ecs
terraform init -backend=false
terraform validate
```

This is what CI runs to keep the example from bit-rotting.

## Tear-down

```bash
terraform destroy -var "powdb_password_secret_arn=…"
```

EFS retention is **not** preserved by `destroy` — your data goes with the
file system. Snapshot first if you care about the contents.
