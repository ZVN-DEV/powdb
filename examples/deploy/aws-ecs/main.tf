# PowDB on AWS ECS Fargate + EFS
#
# Starting point — NOT a turnkey production deploy. Reviewed surfaces:
#   - Default VPC (intentional: keeps the module small; production should
#     wire a dedicated VPC + private subnets + a NAT path).
#   - EFS for POWDB_DATA. EFS gives POSIX semantics across container
#     restarts. fsync latency is higher than EBS/local NVMe — acceptable
#     for the embedded/hobby case the example targets.
#   - One Fargate task. PowDB is single-writer; scaling out means sharding,
#     which is out of scope for this example.
#   - POWDB_PASSWORD is read from Secrets Manager (ARN supplied via var).
#   - POWDB_REQUIRE_TLS defaults on. Bring your own ACM cert ARN if you
#     terminate TLS in front; otherwise mount cert/key into the container
#     via EFS or build them into the image (out of scope here).
#
# Smoke check (no apply):
#   terraform init -backend=false && terraform validate

terraform {
  required_version = ">= 1.5.0"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.0"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

# ─── Network: use the account's default VPC for the example ──────────────
data "aws_vpc" "default" {
  default = true
}

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

# ─── Security group: only allow the PowDB TCP port from the SG itself ────
resource "aws_security_group" "powdb" {
  name        = "${var.name_prefix}-sg"
  description = "PowDB Fargate task — TCP wire protocol"
  vpc_id      = data.aws_vpc.default.id

  ingress {
    description = "PowDB wire protocol"
    from_port   = var.powdb_port
    to_port     = var.powdb_port
    protocol    = "tcp"
    cidr_blocks = var.allowed_cidr_blocks
  }

  # EFS mount target reaches into this SG on 2049 — handled by the EFS SG below.
  egress {
    description = "All egress"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

# ─── EFS for POWDB_DATA ──────────────────────────────────────────────────
resource "aws_efs_file_system" "powdb_data" {
  creation_token   = "${var.name_prefix}-data"
  encrypted        = true
  performance_mode = "generalPurpose"
  throughput_mode  = "bursting"

  lifecycle_policy {
    transition_to_ia = var.efs_transition_to_ia
  }

  tags = {
    Name = "${var.name_prefix}-data"
  }
}

resource "aws_security_group" "efs" {
  name        = "${var.name_prefix}-efs-sg"
  description = "EFS mount targets for PowDB"
  vpc_id      = data.aws_vpc.default.id

  ingress {
    description     = "NFS from PowDB task"
    from_port       = 2049
    to_port         = 2049
    protocol        = "tcp"
    security_groups = [aws_security_group.powdb.id]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_efs_mount_target" "powdb_data" {
  for_each        = toset(data.aws_subnets.default.ids)
  file_system_id  = aws_efs_file_system.powdb_data.id
  subnet_id       = each.value
  security_groups = [aws_security_group.efs.id]
}

# ─── ECS cluster + task ──────────────────────────────────────────────────
resource "aws_ecs_cluster" "powdb" {
  name = "${var.name_prefix}-cluster"
}

resource "aws_iam_role" "task_execution" {
  name = "${var.name_prefix}-task-exec"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Principal = {
        Service = "ecs-tasks.amazonaws.com"
      }
      Action = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy_attachment" "task_execution_managed" {
  role       = aws_iam_role.task_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

# Permission to read the password secret at task start.
resource "aws_iam_role_policy" "secrets_read" {
  name = "${var.name_prefix}-secrets-read"
  role = aws_iam_role.task_execution.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["secretsmanager:GetSecretValue"]
      Resource = [var.powdb_password_secret_arn]
    }]
  })
}

resource "aws_cloudwatch_log_group" "powdb" {
  name              = "/ecs/${var.name_prefix}"
  retention_in_days = 14
}

resource "aws_ecs_task_definition" "powdb" {
  family                   = "${var.name_prefix}-task"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = var.task_cpu
  memory                   = var.task_memory
  execution_role_arn       = aws_iam_role.task_execution.arn

  volume {
    name = "powdb-data"
    efs_volume_configuration {
      file_system_id     = aws_efs_file_system.powdb_data.id
      transit_encryption = "ENABLED"
    }
  }

  container_definitions = jsonencode([{
    name      = "powdb-server"
    image     = var.powdb_image
    essential = true
    portMappings = [{
      containerPort = var.powdb_port
      protocol      = "tcp"
    }]
    environment = [
      { name = "POWDB_PORT", value = tostring(var.powdb_port) },
      { name = "POWDB_BIND", value = "0.0.0.0" },
      { name = "POWDB_DATA", value = "/data" },
      { name = "POWDB_REQUIRE_TLS", value = var.require_tls ? "1" : "0" },
      { name = "POWDB_QUERY_MEMORY_LIMIT", value = tostring(var.query_memory_limit_bytes) },
      { name = "RUST_LOG", value = "info" },
    ]
    secrets = [
      { name = "POWDB_PASSWORD", valueFrom = var.powdb_password_secret_arn },
    ]
    mountPoints = [{
      sourceVolume  = "powdb-data"
      containerPath = "/data"
      readOnly      = false
    }]
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.powdb.name
        awslogs-region        = var.aws_region
        awslogs-stream-prefix = "powdb"
      }
    }
  }])
}

resource "aws_ecs_service" "powdb" {
  name            = "${var.name_prefix}-svc"
  cluster         = aws_ecs_cluster.powdb.id
  task_definition = aws_ecs_task_definition.powdb.arn
  desired_count   = 1
  launch_type     = "FARGATE"

  # PowDB is single-writer — never run 2 tasks against the same EFS dir.
  deployment_maximum_percent         = 100
  deployment_minimum_healthy_percent = 0

  network_configuration {
    subnets          = data.aws_subnets.default.ids
    security_groups  = [aws_security_group.powdb.id]
    assign_public_ip = var.assign_public_ip
  }

  depends_on = [aws_efs_mount_target.powdb_data]
}
