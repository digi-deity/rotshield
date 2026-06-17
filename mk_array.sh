echo ">>> [mk_array] Creating NonRAID array with disk assignments"
nmdctl create --force P:/dev/loop0p1:virtdisk-001 Q:/dev/loop1p1:virtdisk-002 1:/dev/loop2p1:virtdisk-003 2:/dev/loop3p1:virtdisk-004

echo ">>> [mk_array] Starting the NonRAID array"
sudo nmdctl start
sleep 1

echo ">>> [mk_array] Running array check (no correct)"
sudo nmdctl -u check
sleep 3

echo ">>> [mk_array] Mounting array disks"
nmdctl mount
sleep 1

echo ">>> [mk_array] Checking array status"
nmdctl status

echo ">>> [mk_array] Array setup complete"