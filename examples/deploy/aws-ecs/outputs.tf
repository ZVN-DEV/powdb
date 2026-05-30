output "ecs_cluster_arn" {
  description = "ARN of the ECS cluster running PowDB."
  value       = aws_ecs_cluster.powdb.arn
}

output "ecs_service_arn" {
  description = "ARN of the PowDB Fargate service."
  value       = aws_ecs_service.powdb.id
}

output "task_definition_arn" {
  description = "ARN of the PowDB task definition (current revision)."
  value       = aws_ecs_task_definition.powdb.arn
}

output "efs_file_system_id" {
  description = "EFS file system holding POWDB_DATA."
  value       = aws_efs_file_system.powdb_data.id
}

output "security_group_id" {
  description = "Security group attached to the PowDB Fargate task."
  value       = aws_security_group.powdb.id
}

output "powdb_endpoint_hint" {
  description = "How clients should connect. Fargate tasks get an ENI per run; use service discovery, an NLB, or `aws ecs describe-tasks` to resolve the live IP. The port is fixed."
  value       = "powdb-server on tcp/${var.powdb_port} — resolve task ENI via ECS; or front with an NLB and point clients at the NLB DNS."
}
