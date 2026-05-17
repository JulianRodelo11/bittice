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

  ingress {
    description = "REST API"
    from_port   = 3000
    to_port     = 3000
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "Admin API"
    from_port   = 8080
    to_port     = 8080
    protocol    = "tcp"
    cidr_blocks = [var.allowed_admin_cidr]
  }

  ingress {
    description = "gRPC"
    from_port   = 50051
    to_port     = 50051
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
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

# When the wizard discovers an RDS in the same account, it passes the RDS's SG
# here and we open MySQL (3306 by default) from the Bittice EC2's SG. This is
# how Bittice replaces a VPN tunnel with native AWS networking.
resource "aws_security_group_rule" "rds_ingress_from_bittice" {
  count                    = var.target_rds_security_group_id != "" ? 1 : 0
  type                     = "ingress"
  from_port                = var.rds_port
  to_port                  = var.rds_port
  protocol                 = "tcp"
  security_group_id        = var.target_rds_security_group_id
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
