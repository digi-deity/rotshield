echo ">>> [mk_array] Creating NonRAID array with disk assignments"
./nmdctl create --force P:/dev/loop0:virtdisk-001:64 Q:/dev/loop1:virtdisk-002:64 1:/dev/loop2:virtdisk-003:64 2:/dev/loop3:virtdisk-004:64

echo ">>> [mk_array] Starting the NonRAID array"
echo 'y' | nmdctl start
sleep 1

echo ">>> [mk_array] Running array check"
echo 'y' | nmdctl check
sleep 1

echo ">>> [mk_array] Mounting array disks"
nmdctl mount
sleep 1

echo ">>> [mk_array] Checking array status"
nmdctl status

echo ">>> [mk_array] Array setup complete"