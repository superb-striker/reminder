#!/bin/bash
# Provisions the networking (VCN, internet gateway, route table,
# security list, public subnet) and the VM itself via the OCI CLI.
# Requires `oci setup config` to have been run already.
#
# Fill in the variables below, then: bash provision.sh
set -euo pipefail

# Leave as-is to use your tenancy's root compartment (fine if you
# haven't created sub-compartments). Otherwise paste a compartment OCID.
COMPARTMENT_ID="__ROOT__"

# Must already exist -- generate with: ssh-keygen -t ed25519 -f ~/.ssh/reminder-vps
SSH_PUBLIC_KEY_FILE="$HOME/.ssh/reminder-vps.pub"

DISPLAY_PREFIX="reminder"
AD_INDEX=1   # which availability domain to use: 1, 2, or 3

# ---------------------------------------------------------------------

if [ "$COMPARTMENT_ID" = "__ROOT__" ]; then
    COMPARTMENT_ID=$(grep '^tenancy' ~/.oci/config | head -1 | cut -d= -f2 | tr -d ' ')
fi
echo "Using compartment: $COMPARTMENT_ID"

if [ ! -f "$SSH_PUBLIC_KEY_FILE" ]; then
    echo "SSH public key not found at $SSH_PUBLIC_KEY_FILE" >&2
    echo "Generate one with: ssh-keygen -t ed25519 -f ~/.ssh/reminder-vps" >&2
    exit 1
fi

echo "==> Creating VCN"
VCN_ID=$(oci network vcn create \
    --compartment-id "$COMPARTMENT_ID" \
    --cidr-blocks '["10.0.0.0/16"]' \
    --display-name "${DISPLAY_PREFIX}-vcn" \
    --dns-label "remindervcn" \
    --wait-for-state AVAILABLE \
    --query 'data.id' --raw-output)
echo "    VCN: $VCN_ID"

echo "==> Creating internet gateway"
IGW_ID=$(oci network internet-gateway create \
    --compartment-id "$COMPARTMENT_ID" \
    --vcn-id "$VCN_ID" \
    --is-enabled true \
    --display-name "${DISPLAY_PREFIX}-igw" \
    --wait-for-state AVAILABLE \
    --query 'data.id' --raw-output)
echo "    IGW: $IGW_ID"

echo "==> Routing 0.0.0.0/0 through the internet gateway"
RT_ID=$(oci network route-table list \
    --compartment-id "$COMPARTMENT_ID" --vcn-id "$VCN_ID" \
    --query 'data[0].id' --raw-output)
oci network route-table update \
    --rt-id "$RT_ID" \
    --route-rules "[{\"destination\": \"0.0.0.0/0\", \"destinationType\": \"CIDR_BLOCK\", \"networkEntityId\": \"$IGW_ID\"}]" \
    --force
echo "    Route table: $RT_ID"

echo "==> Opening ports 22, 80, 443"
SL_ID=$(oci network security-list list \
    --compartment-id "$COMPARTMENT_ID" --vcn-id "$VCN_ID" \
    --query 'data[0].id' --raw-output)
oci network security-list update \
    --security-list-id "$SL_ID" \
    --ingress-security-rules '[
        {"source":"0.0.0.0/0","protocol":"6","description":"SSH","tcpOptions":{"destinationPortRange":{"min":22,"max":22}}},
        {"source":"0.0.0.0/0","protocol":"6","description":"HTTP","tcpOptions":{"destinationPortRange":{"min":80,"max":80}}},
        {"source":"0.0.0.0/0","protocol":"6","description":"HTTPS","tcpOptions":{"destinationPortRange":{"min":443,"max":443}}}
    ]' \
    --egress-security-rules '[{"destination":"0.0.0.0/0","protocol":"all"}]' \
    --force
echo "    Security list: $SL_ID"

echo "==> Creating public subnet"
SUBNET_ID=$(oci network subnet create \
    --compartment-id "$COMPARTMENT_ID" \
    --vcn-id "$VCN_ID" \
    --cidr-block "10.0.0.0/24" \
    --display-name "${DISPLAY_PREFIX}-public-subnet" \
    --dns-label "remindersub" \
    --route-table-id "$RT_ID" \
    --security-list-ids "[\"$SL_ID\"]" \
    --prohibit-public-ip-on-vnic false \
    --wait-for-state AVAILABLE \
    --query 'data.id' --raw-output)
echo "    Subnet: $SUBNET_ID"

echo "==> Finding availability domain #$AD_INDEX"
AD=$(oci iam availability-domain list \
    --compartment-id "$COMPARTMENT_ID" \
    --query "data[$((AD_INDEX - 1))].name" --raw-output)
echo "    AD: $AD"

echo "==> Finding latest Ubuntu 22.04 ARM image"
IMAGE_ID=$(oci compute image list \
    --compartment-id "$COMPARTMENT_ID" \
    --operating-system "Canonical Ubuntu" \
    --operating-system-version "22.04" \
    --shape "VM.Standard.A1.Flex" \
    --sort-by TIMECREATED --sort-order DESC \
    --query 'data[0].id' --raw-output)
echo "    Image: $IMAGE_ID"

echo "==> Launching instance (takes a couple of minutes)"
INSTANCE_ID=$(oci compute instance launch \
    --compartment-id "$COMPARTMENT_ID" \
    --availability-domain "$AD" \
    --shape "VM.Standard.A1.Flex" \
    --shape-config '{"ocpus":1,"memoryInGBs":6}' \
    --display-name "${DISPLAY_PREFIX}-server" \
    --image-id "$IMAGE_ID" \
    --subnet-id "$SUBNET_ID" \
    --assign-public-ip true \
    --ssh-authorized-keys-file "$SSH_PUBLIC_KEY_FILE" \
    --wait-for-state RUNNING \
    --query 'data.id' --raw-output)
echo "    Instance: $INSTANCE_ID"

PUBLIC_IP=$(oci compute instance list-vnics \
    --instance-id "$INSTANCE_ID" \
    --query 'data[0]."public-ip"' --raw-output)

echo ""
echo "Done. Public IP: $PUBLIC_IP"
echo "SSH in with:"
echo "  ssh -i ${SSH_PUBLIC_KEY_FILE%.pub} ubuntu@$PUBLIC_IP"
echo ""
echo "Continue from step 2 in deploy/README.md (SSH hardening onward) --"
echo "networking and the instance are already done."
