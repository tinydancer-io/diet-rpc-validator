sudo mkdir /mnt/accounts
sudo mkdir /mnt/ledger
sudo mkdir /mnt/snapshots

sudo mount -o size=80G -t tmpfs none /mnt/accounts
sudo mount -o size=80G -t tmpfs none /mnt/snapshots
sudo mount -o size=80G -t tmpfs none /mnt/ledger

sudo ls -la /mnt