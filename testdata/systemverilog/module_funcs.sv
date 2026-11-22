module top(input logic a, output logic b);
  function automatic int addone(input int value);
    return value + 1;
  endfunction

  task automatic dowork(input int value);
    $display("%0d", value);
  endtask
endmodule
