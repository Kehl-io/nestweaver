`include "common_defs.svh"

import uvm_pkg::*;
import bus_pkg::BusTransaction;

module top_module #(
  parameter WIDTH = 8
)(
  input  logic clk,
  input  logic rst_n,
  output logic [WIDTH-1:0] data_out
);

  logic [WIDTH-1:0] counter;

  always_ff @(posedge clk or negedge rst_n) begin
    if (!rst_n)
      counter <= '0;
    else
      counter <= counter + 1;
  end

  assign data_out = counter;

  sub_module #(.WIDTH(WIDTH)) u_sub (
    .clk(clk),
    .data(counter)
  );

endmodule

interface axi_if #(
  parameter ADDR_WIDTH = 32,
  parameter DATA_WIDTH = 64
);
  logic [ADDR_WIDTH-1:0] addr;
  logic [DATA_WIDTH-1:0] data;
  logic valid;
  logic ready;

  modport master(output addr, data, valid, input ready);
  modport slave(input addr, data, valid, output ready);
endinterface

class packet extends base_packet;
  rand bit [7:0] payload[];
  int length;

  function new(string name = "packet");
    super.new(name);
  endfunction

  function void build();
    payload = new[length];
  endfunction

  task send(input int port);
    // send packet on port
  endtask
endclass

function automatic int compute_checksum(input bit [7:0] data[], input int len);
  int sum = 0;
  for (int i = 0; i < len; i++) begin
    sum += data[i];
  end
  return sum;
endfunction

module sub_module #(
  parameter WIDTH = 8
)(
  input  logic clk,
  input  logic [WIDTH-1:0] data
);
  // sub-module implementation
endmodule
