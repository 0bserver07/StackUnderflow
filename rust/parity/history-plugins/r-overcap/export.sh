i=0
while [ $i -lt 200 ]; do printf '%01024d' 0; i=$((i+1)); done
