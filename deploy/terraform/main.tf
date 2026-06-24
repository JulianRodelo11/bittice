# Use the pre-existing IAM instance profile (created manually — requires iam:CreateRole
# which is not available to the deploying user). The profile must have
# CloudWatchAgentServerPolicy attached so the CloudWatch agent can publish metrics.

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-*"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

resource "aws_key_pair" "bittice" {
  key_name   = "${var.app_name}-key"
  public_key = var.ssh_public_key
}

resource "aws_security_group" "bittice" {
  name        = "${var.app_name}-sg"
  description = "Bittice EC2 security group"

  # When target_vpc_id is set, the SG lives in the same VPC as the RDS so we can
  # reference it from the RDS's SG rule below. When empty, AWS picks the default VPC.
  vpc_id = var.target_vpc_id != "" ? var.target_vpc_id : null

  ingress {
    description = "SSH"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.allowed_ssh_cidr]
  }

  # REST is served on 443 via Caddy → bittice:3000 (no public :3000).
  ingress {
    description = "HTTPS (REST via Caddy)"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "HTTP (ACME certificate issuance)"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "Admin API (VPC or SSH tunnel only)"
    from_port   = 8080
    to_port     = 8080
    protocol    = "tcp"
    cidr_blocks = [var.allowed_admin_cidr]
  }

  ingress {
    description = "gRPC (VPC internal only)"
    from_port   = 50051
    to_port     = 50051
    protocol    = "tcp"
    cidr_blocks = [var.allowed_grpc_cidr]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Name = "${var.app_name}-sg"
  }
}

# For each target RDS the wizard discovered, open inbound MySQL from the
# Bittice EC2's SG. One rule per target — Bittice replaces VPN with native
# intra-VPC SG-to-SG networking.
resource "aws_security_group_rule" "rds_ingress_from_bittice" {
  count                    = length(var.target_rds_security_group_ids)
  type                     = "ingress"
  from_port                = var.rds_port
  to_port                  = var.rds_port
  protocol                 = "tcp"
  security_group_id        = var.target_rds_security_group_ids[count.index]
  source_security_group_id = aws_security_group.bittice.id
  description              = "Bittice CDC binlog stream (managed by ${var.app_name} Terraform)"
}

resource "aws_instance" "bittice" {
  ami                    = data.aws_ami.ubuntu.id
  instance_type          = var.instance_type
  key_name               = aws_key_pair.bittice.key_name
  vpc_security_group_ids = [aws_security_group.bittice.id]
  subnet_id              = var.target_subnet_id != "" ? var.target_subnet_id : null

  user_data = <<-EOF
    #!/bin/bash
    set -euxo pipefail
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y docker.io
    systemctl enable --now docker
    usermod -aG docker ubuntu
    mkdir -p /opt/bittice/data
    chown ubuntu:ubuntu /opt/bittice/data
  EOF

  root_block_device {
    volume_size = var.data_volume_size_gb
    volume_type = "gp3"
    encrypted   = true
  }

  # IMDSv2 with hop_limit=2 so the motor (inside a Docker container, behind
  # the default bridge network) can still reach 169.254.169.254. With the
  # AWS default of hop_limit=1, the bridge router decrements TTL and drops
  # the request, IMDS returns nothing, and the motor reports `instance_type
  # = null` to the control plane on every heartbeat — billing and the
  # dashboard then keep showing whatever value was set at deploy time and
  # never reflect resizes.  Verified on 2026-05-25 on this same instance:
  # after stopping/starting from t3.micro to t3.nano, deployments.instance_type
  # stayed at t3.micro until hop_limit was bumped + the container restarted.
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 2
  }

  tags = {
    Name = var.app_name
  }

  lifecycle {
    # Prevent accidental replacement that would destroy data volume
    ignore_changes = [user_data, ami]
  }
}

resource "aws_eip" "bittice" {
  instance = aws_instance.bittice.id
  domain   = "vpc"

  tags = {
    Name = "${var.app_name}-eip"
  }
}
